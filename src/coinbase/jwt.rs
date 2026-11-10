//! Coinbase CDP JSON Web Tokens: ES256 (a P-256 key in a PEM file) or EdDSA (a base64 Ed25519
//! key). One token per request; they are valid for two minutes and never reused.
//!
//! Claims, from Coinbase's authentication documentation (docs.cdp.coinbase.com, checked
//! 2026-11-09): `sub` the key name, `iss` `cdp`, `nbf` now, `exp` now + 120 s, and for REST
//! `uri` = `"METHOD host/path"` with no scheme. Headers carry `kid` (the key name) and a random
//! `nonce`. A WebSocket token has no `uri`.
//!
//! Not verified against the live service: whether a query string belongs in `uri` (the documented
//! examples have none, so none is signed), and whether Ed25519 keys are accepted by every
//! endpoint (the documentation is inconsistent). Both are recorded in `docs/coinbase.md`.

use std::fmt;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair};
use serde_json::json;

use crate::adapter::Clock;
use crate::auth::der::{self, Sec1Error};
use crate::auth::{RequestParts, RequestSigner};
use crate::error::{BrokerError, ErrorKind, Result};

/// How long a token lives, in seconds. Coinbase rejects longer.
const LIFETIME_SECS: i64 = 120;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Algorithm {
    /// ECDSA over P-256 with SHA-256.
    Es256,
    /// Ed25519.
    EdDsa,
}

enum Key {
    Ecdsa(EcdsaKeyPair),
    Ed25519(Ed25519KeyPair),
}

/// Signs requests with a CDP API key.
pub struct JwtSigner {
    name: String,
    key: Key,
    clock: Arc<dyn Clock>,
    rng: SystemRandom,
}

impl fmt::Debug for JwtSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JwtSigner")
            .field("algorithm", &self.algorithm())
            .field("key", &"<redacted>")
            .finish()
    }
}

fn bad_key(why: &str) -> BrokerError {
    BrokerError::new(
        ErrorKind::Auth,
        format!("The private key could not be read: {why}"),
    )
}

impl JwtSigner {
    /// `name` is the key name (`organizations/{org}/apiKeys/{id}`); `secret` is the private key as
    /// Coinbase shows it. Neither is ever included in an error.
    pub fn new(name: &str, secret: &str, clock: Arc<dyn Clock>) -> Result<Self> {
        let name = name.trim();
        if name.is_empty() {
            return Err(BrokerError::new(
                ErrorKind::Auth,
                "The API key name is missing.",
            ));
        }
        let rng = SystemRandom::new();
        let key = parse_secret(secret, &rng)?;
        Ok(Self {
            name: name.to_owned(),
            key,
            clock,
            rng,
        })
    }

    pub fn algorithm(&self) -> Algorithm {
        match self.key {
            Key::Ecdsa(_) => Algorithm::Es256,
            Key::Ed25519(_) => Algorithm::EdDsa,
        }
    }

    /// A token for one REST request.
    pub fn sign_rest(&self, method: &str, host: &str, path: &str) -> Result<String> {
        self.sign(Some(format!(
            "{} {host}{path}",
            method.to_ascii_uppercase()
        )))
    }

    /// A token for a WebSocket subscription (no `uri`).
    pub fn sign_websocket(&self) -> Result<String> {
        self.sign(None)
    }

    fn sign(&self, uri: Option<String>) -> Result<String> {
        let now = self.clock.now_ms().div_euclid(1000);
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|_| BrokerError::new(ErrorKind::Transient, "No random numbers available."))?;
        let algorithm = match self.algorithm() {
            Algorithm::Es256 => "ES256",
            Algorithm::EdDsa => "EdDSA",
        };
        let header = json!({
            "alg": algorithm,
            "kid": self.name,
            "nonce": hex(&nonce),
            "typ": "JWT",
        });
        let mut claims = json!({
            "sub": self.name,
            "iss": "cdp",
            "nbf": now,
            "exp": now + LIFETIME_SECS,
        });
        if let Some(uri) = uri {
            claims["uri"] = json!(uri);
        }
        if self.algorithm() == Algorithm::EdDsa {
            // Coinbase's Ed25519 samples add the audience; the ECDSA ones do not.
            claims["aud"] = json!(["cdp_service"]);
        }
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = match &self.key {
            Key::Ecdsa(pair) => pair
                .sign(&self.rng, signing_input.as_bytes())
                .map_err(|_| BrokerError::new(ErrorKind::Auth, "Signing failed."))?
                .as_ref()
                .to_vec(),
            Key::Ed25519(pair) => pair.sign(signing_input.as_bytes()).as_ref().to_vec(),
        };
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }
}

impl RequestSigner for JwtSigner {
    fn headers(&self, request: &RequestParts<'_>) -> Result<Vec<(String, String)>> {
        let token = self.sign_rest(request.method, request.host, request.path)?;
        Ok(vec![(
            "Authorization".to_owned(),
            format!("Bearer {token}"),
        )])
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The PEM blocks in `text`: `(label, der)`.
fn pem_blocks(text: &str) -> Vec<(String, Vec<u8>)> {
    let mut blocks = Vec::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(label) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|rest| rest.strip_suffix("-----"))
        else {
            continue;
        };
        let end = format!("-----END {label}-----");
        let mut body = String::new();
        for line in lines.by_ref() {
            if line == end {
                if let Ok(der) = STANDARD.decode(body.as_bytes()) {
                    blocks.push((label.to_owned(), der));
                }
                break;
            }
            body.push_str(line);
        }
    }
    blocks
}

fn parse_secret(secret: &str, rng: &SystemRandom) -> Result<Key> {
    // Coinbase shows the key with `\n` escapes in some places; accept both forms.
    let text = secret.trim().replace("\\n", "\n");
    if text.is_empty() {
        return Err(bad_key("it is empty."));
    }
    if text.contains("-----BEGIN") {
        let blocks = pem_blocks(&text);
        let Some((label, der)) = blocks.first() else {
            return Err(bad_key("the PEM block is incomplete."));
        };
        return match label.as_str() {
            "EC PRIVATE KEY" => {
                let key = der::parse_sec1_p256(der).map_err(|error| match error {
                    Sec1Error::Malformed => bad_key("it is not a valid EC private key."),
                    Sec1Error::WrongCurve => bad_key("only P-256 (prime256v1) keys are supported."),
                    Sec1Error::NoPublicKey => bad_key("it does not contain the public key."),
                })?;
                EcdsaKeyPair::from_private_key_and_public_key(
                    &ECDSA_P256_SHA256_FIXED_SIGNING,
                    key.private,
                    key.public,
                    rng,
                )
                .map(Key::Ecdsa)
                .map_err(|_| bad_key("it is not a valid P-256 key."))
            }
            "PRIVATE KEY" => EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der, rng)
                .map(Key::Ecdsa)
                .or_else(|_| Ed25519KeyPair::from_pkcs8_maybe_unchecked(der).map(Key::Ed25519))
                .map_err(|_| bad_key("it is neither a P-256 nor an Ed25519 key.")),
            _ => Err(bad_key(
                "expected an EC PRIVATE KEY block (or a base64 Ed25519 key).",
            )),
        };
    }
    // No PEM: Coinbase's Ed25519 keys are base64 of the 32-byte seed followed by the 32-byte
    // public key.
    let raw = STANDARD
        .decode(text.as_bytes())
        .map_err(|_| bad_key("it is neither a PEM block nor base64."))?;
    match raw.len() {
        32 | 64 => Ed25519KeyPair::from_seed_unchecked(&raw[..32])
            .map(Key::Ed25519)
            .map_err(|_| bad_key("it is not a valid Ed25519 key.")),
        _ => Err(bad_key("an Ed25519 key is 32 or 64 bytes of base64.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, ED25519, KeyPair, UnparsedPublicKey};

    struct FixedClock(i64);
    impl Clock for FixedClock {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    const NOW_MS: i64 = 1_800_000_123_456;

    fn clock() -> Arc<dyn Clock> {
        Arc::new(FixedClock(NOW_MS))
    }

    fn pem(label: &str, der: &[u8]) -> String {
        let body = STANDARD.encode(der);
        let wrapped: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect();
        format!(
            "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
            wrapped.join("\n")
        )
    }

    /// A fresh P-256 key as (PKCS#8 DER, SEC1 DER, public key bytes).
    fn p256() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let rng = SystemRandom::new();
        let document =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pkcs8 = document.as_ref().to_vec();
        let pair =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &rng).unwrap();
        // PKCS#8: SEQUENCE { INTEGER 0, SEQUENCE { OID, OID }, OCTET STRING { ECPrivateKey } }.
        let outer = der::read(&pkcs8).unwrap();
        let version = der::read(outer.content).unwrap();
        let algorithm = der::read(version.rest).unwrap();
        let wrapped = der::read(algorithm.rest).unwrap();
        assert_eq!(wrapped.tag, der::OCTET_STRING);
        let sec1 = wrapped.content.to_vec();
        (pkcs8, sec1, pair.public_key().as_ref().to_vec())
    }

    fn split(token: &str) -> (String, serde_json::Value, serde_json::Value, Vec<u8>) {
        let mut parts = token.split('.');
        let header = parts.next().unwrap();
        let payload = parts.next().unwrap();
        let signature = parts.next().unwrap();
        assert!(parts.next().is_none());
        let decode = |part: &str| -> serde_json::Value {
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
        };
        (
            format!("{header}.{payload}"),
            decode(header),
            decode(payload),
            URL_SAFE_NO_PAD.decode(signature).unwrap(),
        )
    }

    #[test]
    fn an_es256_token_has_the_documented_claims_and_a_valid_signature() {
        let (_, sec1, public) = p256();
        let name = "organizations/org-1/apiKeys/key-1";
        let signer = JwtSigner::new(name, &pem("EC PRIVATE KEY", &sec1), clock()).unwrap();
        assert_eq!(signer.algorithm(), Algorithm::Es256);
        let token = signer
            .sign_rest("get", "api.coinbase.com", "/api/v3/brokerage/accounts")
            .unwrap();
        let (input, header, claims, signature) = split(&token);
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], name);
        assert_eq!(header["typ"], "JWT");
        assert_eq!(header["nonce"].as_str().unwrap().len(), 32);
        assert_eq!(claims["sub"], name);
        assert_eq!(claims["iss"], "cdp");
        assert_eq!(claims["nbf"], 1_800_000_123_i64);
        assert_eq!(claims["exp"], 1_800_000_243_i64);
        assert_eq!(
            claims["uri"],
            "GET api.coinbase.com/api/v3/brokerage/accounts"
        );
        assert!(claims.get("aud").is_none());
        assert_eq!(signature.len(), 64, "fixed-width r||s, as JWT requires");
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &public)
            .verify(input.as_bytes(), &signature)
            .expect("the signature verifies with the key's public half");
    }

    #[test]
    fn a_pkcs8_pem_is_accepted_too_and_escaped_newlines_are_unescaped() {
        let (pkcs8, _, public) = p256();
        let text = pem("PRIVATE KEY", &pkcs8).replace('\n', "\\n");
        let signer = JwtSigner::new("k", &text, clock()).unwrap();
        let (input, _, _, signature) = split(&signer.sign_websocket().unwrap());
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, &public)
            .verify(input.as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn an_ed25519_key_in_coinbases_base64_form_signs_with_eddsa() {
        let rng = SystemRandom::new();
        let document = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pkcs8 = document.as_ref();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8).unwrap();
        // PKCS#8 v2: SEQUENCE { INTEGER 1, SEQUENCE { OID }, OCTET STRING { OCTET STRING seed }, ... }.
        let outer = der::read(pkcs8).unwrap();
        let version = der::read(outer.content).unwrap();
        let algorithm = der::read(version.rest).unwrap();
        let wrapped = der::read(algorithm.rest).unwrap();
        let seed = der::read(wrapped.content).unwrap().content;
        assert_eq!(seed.len(), 32);
        let mut combined = seed.to_vec();
        combined.extend_from_slice(pair.public_key().as_ref());
        let secret = STANDARD.encode(&combined);

        let signer = JwtSigner::new("k", &secret, clock()).unwrap();
        assert_eq!(signer.algorithm(), Algorithm::EdDsa);
        let (input, header, claims, signature) = split(&signer.sign_websocket().unwrap());
        assert_eq!(header["alg"], "EdDSA");
        assert_eq!(claims["aud"][0], "cdp_service");
        assert!(claims.get("uri").is_none(), "a WebSocket token has no uri");
        UnparsedPublicKey::new(&ED25519, pair.public_key().as_ref())
            .verify(input.as_bytes(), &signature)
            .unwrap();
        // The bare 32-byte seed works as well.
        assert!(JwtSigner::new("k", &STANDARD.encode(seed), clock()).is_ok());
    }

    #[test]
    fn every_token_has_a_fresh_nonce() {
        let (_, sec1, _) = p256();
        let signer = JwtSigner::new("k", &pem("EC PRIVATE KEY", &sec1), clock()).unwrap();
        let a = split(&signer.sign_websocket().unwrap()).1["nonce"].clone();
        let b = split(&signer.sign_websocket().unwrap()).1["nonce"].clone();
        assert_ne!(a, b);
    }

    #[test]
    fn bad_keys_are_refused_without_echoing_them() {
        let secret_looking = "SUPERSECRETVALUE-not-a-key";
        for (case, bad) in [
            "",
            secret_looking,
            "-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----\n",
            "-----BEGIN EC PRIVATE KEY-----\nAAAA",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----\n",
            "AAAA",
        ]
        .into_iter()
        .enumerate()
        {
            let error = JwtSigner::new("k", bad, clock()).unwrap_err();
            // The case number, not the value: a key-shaped string must not reach a test log.
            assert_eq!(error.kind, ErrorKind::Auth, "bad key case #{case}");
            assert!(!error.to_string().contains(secret_looking));
            assert!(!error.detail.contains(secret_looking));
        }
        assert_eq!(
            JwtSigner::new("  ", "AAAA", clock()).unwrap_err().kind,
            ErrorKind::Auth
        );
    }

    #[test]
    fn an_rsa_or_other_curve_sec1_key_is_not_mistaken_for_p256() {
        // SEQUENCE { INTEGER 1, OCTET STRING (32), [0] OID secp256k1 } - wrong curve, no public key.
        let mut der = vec![0x30, 0x34, 0x02, 0x01, 0x01, 0x04, 0x20];
        der.extend([9_u8; 32]);
        der.extend([0xa0, 0x07, 0x06, 0x05, 0x2b, 0x81, 0x04, 0x00, 0x0a]);
        // Fix the outer length to what we actually built.
        let body = der[2..].to_vec();
        let mut full = vec![0x30, body.len() as u8];
        full.extend(body);
        let error = JwtSigner::new("k", &pem("EC PRIVATE KEY", &full), clock()).unwrap_err();
        assert!(error.to_string().contains("P-256"), "{error}");
    }

    #[test]
    fn the_signer_does_not_print_its_key() {
        let (_, sec1, _) = p256();
        let signer = JwtSigner::new(
            "organizations/o/apiKeys/k",
            &pem("EC PRIVATE KEY", &sec1),
            clock(),
        )
        .unwrap();
        let shown = format!("{signer:?}");
        assert!(!shown.contains("organizations"));
        assert!(shown.contains("redacted"));
    }
}
