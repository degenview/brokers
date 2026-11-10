//! The one place this crate talks HTTP.
//!
//! * Redirects are never followed: a signed request must not be replayed at a host the broker (or
//!   anyone in between) chose.
//! * A read is retried (twice, with jittered backoff, honouring `Retry-After`); a write never is.
//!   A write that dies on the wire is [`ErrorKind::OutcomeUnknown`] and the caller must look the
//!   order up before doing anything else.
//! * Every request is signed fresh, spaced by the broker's minimum gap, and every error detail is
//!   scrubbed of credentials before it leaves this module.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::auth::{RequestParts, RequestSigner};
use crate::error::{BrokerError, ErrorKind, Result};
use crate::scrub::Scrubber;

const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;
/// A read is attempted this many times in all.
const READ_ATTEMPTS: usize = 3;
/// The longest we wait because a broker asked us to, per attempt.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(5);
const DETAIL_LIMIT: usize = 500;

/// Whether `url` names a loopback host exactly (`127.0.0.1`, `localhost`, `[::1]`).
pub fn is_loopback_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .or_else(|| url.strip_prefix("ws://"))
        .or_else(|| url.strip_prefix("wss://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        stripped.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// `host[:port]` of an `http(s)://host[:port]/...` URL.
fn authority_of(url: &str) -> &str {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// Percent-encodes a query component.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// `?a=1&b=2` (empty when there are no pairs). A repeated key repeats in the output, which is how
/// array parameters are sent.
pub fn query_string(pairs: &[(&str, &str)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let joined: Vec<String> = pairs
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect();
    format!("?{}", joined.join("&"))
}

/// Spaces calls at least `gap` apart.
pub struct RateLimiter {
    gap: Duration,
    next: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(gap: Duration) -> Self {
        Self {
            gap,
            next: Mutex::new(Instant::now()),
        }
    }

    /// Blocks until this call's slot arrives, then reserves the next one.
    pub fn wait(&self) {
        let slot = {
            let Ok(mut next) = self.next.lock() else {
                return;
            };
            let now = Instant::now();
            let slot = (*next).max(now);
            *next = slot + self.gap;
            slot
        };
        let now = Instant::now();
        if slot > now {
            std::thread::sleep(slot - now);
        }
    }
}

type Sleeper = dyn Fn(Duration) + Send + Sync;

pub struct HttpClient {
    agent: ureq::Agent,
    base: String,
    authority: String,
    signer: Arc<dyn RequestSigner>,
    scrubber: Scrubber,
    limiter: RateLimiter,
    sleeper: Box<Sleeper>,
}

impl HttpClient {
    /// `base` is `https://host` with no trailing slash.
    pub fn new(
        base: &str,
        signer: Arc<dyn RequestSigner>,
        scrubber: Scrubber,
        min_gap: Duration,
    ) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .user_agent(concat!("degenview-brokers/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            base: base.trim_end_matches('/').to_owned(),
            authority: authority_of(base).to_owned(),
            signer,
            scrubber,
            limiter: RateLimiter::new(min_gap),
            sleeper: Box::new(std::thread::sleep),
        }
    }

    /// Replaces how retries wait, so tests do not.
    #[must_use]
    pub fn with_sleeper(mut self, sleeper: impl Fn(Duration) + Send + Sync + 'static) -> Self {
        self.sleeper = Box::new(sleeper);
        self
    }

    pub fn scrubber(&self) -> &Scrubber {
        &self.scrubber
    }

    /// The host (and port) tokens are made for.
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// A signed GET returning the JSON body. Retried on transient failure.
    pub fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        self.read("GET", path, query, None)
    }

    /// A signed POST that **changes nothing** (a preview). Treated and retried like a read.
    pub fn post_read(&self, path: &str, body: &Value) -> Result<Value> {
        self.read("POST", path, &[], Some(body))
    }

    fn read(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value> {
        let mut last = None;
        for attempt in 0..READ_ATTEMPTS {
            match self.once(method, path, query, body, false) {
                Ok(value) => return Ok(value),
                Err(error) if error.is_transient() && attempt + 1 < READ_ATTEMPTS => {
                    let wait = retry_wait(&error, attempt, jitter_seed());
                    last = Some(error);
                    (self.sleeper)(wait);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| BrokerError::new(ErrorKind::Transient, "The request failed.")))
    }

    /// A signed POST with a JSON body that **changes state**. Never retried. A failure to complete
    /// the exchange, a reply that cannot be read, a redirect or any status that is not a clear
    /// refusal is [`ErrorKind::OutcomeUnknown`]: the broker may have acted. Only 400, 401, 403, 409
    /// and 422 (and 404/429, which are not treated as proof either way) say anything else, so an
    /// [`ErrorKind::Invalid`] from a write means that no request left the machine.
    pub fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.once("POST", path, &[], Some(body), true)
    }

    fn once(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
        write: bool,
    ) -> Result<Value> {
        // Sign after the wait: a token's two minutes should not be spent queueing.
        self.limiter.wait();
        let headers = self.signer.headers(&RequestParts {
            method,
            host: &self.authority,
            path,
        })?;
        let url = format!("{}{}{}", self.base, path, query_string(query));

        let result = if let Some(body) = body {
            let mut request = self.agent.post(&url).header("Accept", "application/json");
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            request
                .header("Content-Type", "application/json")
                .send(body.to_string().as_bytes())
        } else {
            let mut request = self.agent.get(&url).header("Accept", "application/json");
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            request.call()
        };

        let mut response = match result {
            Ok(response) => response,
            Err(error) => return Err(self.transport_error(method, path, &error, write)),
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(|seconds| seconds.saturating_mul(1000));
        let text = match response
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
        {
            Ok(text) => text,
            Err(error) => return Err(self.transport_error(method, path, &error, write)),
        };

        if (200..300).contains(&status) {
            return serde_json::from_str(&text).map_err(|_| {
                let detail = self.detail(method, path, status, "body is not JSON");
                if write {
                    // The broker answered success and we cannot read what it said: it may well
                    // have acted. Never "not sent".
                    BrokerError::new(
                        ErrorKind::OutcomeUnknown,
                        "The broker answered, but the reply could not be read. The order may have been placed.",
                    )
                    .with_detail(detail)
                } else {
                    BrokerError::new(
                        ErrorKind::Invalid,
                        "The broker sent a reply that could not be read.",
                    )
                    .with_detail(detail)
                }
            });
        }
        Err(self.status_error(method, path, status, retry_after, &text, write))
    }

    fn detail(&self, method: &str, path: &str, status: u16, note: &str) -> String {
        self.scrubber
            .scrub(&format!("{method} {path} -> HTTP {status}: {note}"))
    }

    fn transport_error(
        &self,
        method: &str,
        path: &str,
        error: &ureq::Error,
        write: bool,
    ) -> BrokerError {
        let detail = self.scrubber.scrub(&format!("{method} {path}: {error}"));
        if write {
            BrokerError::new(
                ErrorKind::OutcomeUnknown,
                "The connection failed while sending the order. It may or may not have been placed.",
            )
            .with_detail(detail)
        } else {
            BrokerError::new(ErrorKind::Transient, "Could not reach the broker.")
                .with_detail(detail)
        }
    }

    fn status_error(
        &self,
        method: &str,
        path: &str,
        status: u16,
        retry_after_ms: Option<u64>,
        body: &str,
        write: bool,
    ) -> BrokerError {
        let (message, code) = error_fields(body);
        let note = message
            .clone()
            .unwrap_or_else(|| body.chars().take(DETAIL_LIMIT).collect());
        let note: String = note.chars().take(DETAIL_LIMIT).collect();
        let detail = self.detail(method, path, status, &note);
        let error = match status {
            401 => BrokerError::new(
                ErrorKind::Auth,
                "The broker rejected the credentials. The key may be wrong, expired or revoked.",
            ),
            403 => BrokerError::new(
                ErrorKind::Permission,
                "These credentials do not have permission for that.",
            ),
            404 => BrokerError::new(ErrorKind::NotFound, "The broker could not find that."),
            429 => BrokerError::new(
                ErrorKind::RateLimited { retry_after_ms },
                "The broker is rate limiting requests. Wait a moment.",
            ),
            // A write that was answered with a redirect reached the broker; refusing to follow it
            // does not say what the broker did first.
            300..=399 if write => BrokerError::new(
                ErrorKind::OutcomeUnknown,
                "The broker redirected the order, which is refused. It may or may not have been placed.",
            ),
            300..=399 => BrokerError::new(
                ErrorKind::Invalid,
                "The broker redirected the request, which is refused.",
            ),
            400 | 409 | 422 => BrokerError::new(
                ErrorKind::Rejected,
                message
                    .map(|m| self.scrubber.scrub(&m))
                    .unwrap_or_else(|| "The broker refused the request.".to_owned()),
            ),
            500..=599 if write => BrokerError::new(
                // A 5xx on a write does not say the order was not placed.
                ErrorKind::OutcomeUnknown,
                "The broker failed while handling the order. It may or may not have been placed.",
            ),
            500..=599 => {
                BrokerError::new(ErrorKind::Transient, "The broker had a problem. Try again.")
            }
            _ if write => BrokerError::new(
                ErrorKind::OutcomeUnknown,
                "The broker sent an unexpected reply to the order. It may or may not have been placed.",
            ),
            _ => BrokerError::new(ErrorKind::Invalid, "The broker sent an unexpected reply."),
        };
        let error = error.with_detail(detail);
        match code {
            Some(code) => error.with_code(self.scrubber.scrub(&code)),
            None => error,
        }
    }
}

/// The broker's `message` and `code`/`error` fields from an error body, when it sent JSON.
fn error_fields(body: &str) -> (Option<String>, Option<String>) {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(body) else {
        return (None, None);
    };
    let text = |key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    (
        text("message").or_else(|| text("error_description")),
        text("code").or_else(|| text("error")),
    )
}

fn retry_wait(error: &BrokerError, attempt: usize, random: u32) -> Duration {
    if let ErrorKind::RateLimited {
        retry_after_ms: Some(ms),
    } = error.kind
    {
        return Duration::from_millis(ms).min(MAX_RETRY_WAIT);
    }
    let base_ms = 250_u64.saturating_mul(1_u64 << attempt.min(4));
    let jitter = 75 + u64::from(random % 51); // 75-125% of the base.
    Duration::from_millis(base_ms * jitter / 100).min(MAX_RETRY_WAIT)
}

fn jitter_seed() -> u32 {
    let mut bytes = [0_u8; 4];
    if getrandom::fill(&mut bytes).is_ok() {
        u32::from_le_bytes(bytes)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::mock::{MockServer, Reply};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StaticSigner;
    impl RequestSigner for StaticSigner {
        fn headers(&self, request: &RequestParts<'_>) -> Result<Vec<(String, String)>> {
            Ok(vec![(
                "Authorization".to_owned(),
                format!(
                    "Bearer test-{}-{}-{}",
                    request.method, request.host, request.path
                ),
            )])
        }
    }

    fn client(server: &MockServer) -> HttpClient {
        HttpClient::new(
            &server.url,
            Arc::new(StaticSigner),
            Scrubber::new(["hunter2-secret"]),
            Duration::ZERO,
        )
        .with_sleeper(|_| {})
    }

    #[test]
    fn loopback_urls_are_recognised_exactly() {
        for yes in [
            "http://127.0.0.1:8080",
            "http://127.0.0.1",
            "http://localhost:1/x",
            "https://localhost",
            "http://[::1]:9",
            "ws://127.0.0.1:7",
        ] {
            assert!(is_loopback_url(yes), "{yes}");
        }
        for no in [
            "https://api.example.com",
            "http://127.0.0.1.evil.example.com",
            "http://localhost.evil.example.com",
            "http://evil@127.0.0.1",
            "http://127.0.0.1@evil.example.com",
            "ftp://127.0.0.1",
            "wss://stream.example.com",
            "127.0.0.1",
            "",
        ] {
            assert!(!is_loopback_url(no), "{no}");
        }
    }

    #[test]
    fn queries_are_encoded_and_arrays_repeat() {
        assert_eq!(query_string(&[]), "");
        assert_eq!(
            query_string(&[
                ("product_ids", "BTC-USD"),
                ("product_ids", "ETH-USD"),
                ("q", "a b&c")
            ]),
            "?product_ids=BTC-USD&product_ids=ETH-USD&q=a%20b%26c"
        );
    }

    #[test]
    fn a_get_is_signed_with_the_path_and_decoded() {
        let server = MockServer::start(|_| Reply::json(200, r#"{"ok":true}"#));
        let value = client(&server).get("/v/things", &[("limit", "5")]).unwrap();
        assert_eq!(value["ok"], true);
        let seen = &server.requests()[0];
        assert_eq!(seen.target, "/v/things?limit=5");
        let auth = seen.header("authorization").unwrap();
        assert!(auth.starts_with("Bearer test-GET-127.0.0.1:"));
        // The signed path carries no query string.
        assert!(auth.ends_with("-/v/things"));
        assert!(
            seen.header("user-agent")
                .unwrap()
                .starts_with("degenview-brokers/")
        );
    }

    #[test]
    fn statuses_map_to_kinds_and_errors_are_scrubbed() {
        for (status, kind) in [
            (401, ErrorKind::Auth),
            (403, ErrorKind::Permission),
            (404, ErrorKind::NotFound),
            (400, ErrorKind::Rejected),
            (500, ErrorKind::Transient),
            (302, ErrorKind::Invalid),
        ] {
            let server = MockServer::start(move |_| {
                Reply::json(
                    status,
                    r#"{"message":"bad hunter2-secret","code":"X_CODE"}"#,
                )
            });
            let error = client(&server).get("/x", &[]).unwrap_err();
            assert_eq!(error.kind, kind, "status {status}");
            assert!(!error.detail.contains("hunter2-secret"), "{}", error.detail);
            assert!(!error.to_string().contains("hunter2-secret"));
        }
        let server = MockServer::start(|_| {
            Reply::json(400, r#"{"message":"Size too small","code":"INVALID_SIZE"}"#)
        });
        let error = client(&server).get("/x", &[]).unwrap_err();
        assert_eq!(error.to_string(), "Size too small");
        assert_eq!(error.code.as_deref(), Some("INVALID_SIZE"));
    }

    #[test]
    fn reads_are_retried_and_honour_retry_after() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let server = MockServer::start(move |_| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                Reply::json(429, "{}").with_header("Retry-After", "3")
            } else {
                Reply::json(200, r#"{"ok":1}"#)
            }
        });
        let waits = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&waits);
        let http = client(&server).with_sleeper(move |d| recorded.lock().unwrap().push(d));
        assert_eq!(http.get("/x", &[]).unwrap()["ok"], 1);
        assert_eq!(server.request_count(), 2);
        assert_eq!(waits.lock().unwrap().as_slice(), [Duration::from_secs(3)]);
    }

    #[test]
    fn a_read_gives_up_after_its_attempts() {
        let server = MockServer::start(|_| Reply::json(503, "{}"));
        let error = client(&server).get("/x", &[]).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Transient);
        assert_eq!(server.request_count(), READ_ATTEMPTS);
    }

    #[test]
    fn a_write_is_never_retried_and_an_unfinished_one_is_outcome_unknown() {
        let server = MockServer::start(|_| Reply::Drop);
        let error = client(&server)
            .post("/orders", &serde_json::json!({"a": 1}))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::OutcomeUnknown);
        assert_eq!(server.request_count(), 1, "a POST is sent once");

        let server = MockServer::start(|_| Reply::json(502, "{}"));
        let error = client(&server)
            .post("/orders", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(
            error.kind,
            ErrorKind::OutcomeUnknown,
            "a 5xx does not prove the order was refused"
        );
        assert_eq!(server.request_count(), 1);

        let server = MockServer::start(|_| Reply::json(400, r#"{"message":"no"}"#));
        let error = client(&server)
            .post("/orders", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Rejected, "a 4xx is a clear refusal");
    }

    #[test]
    fn a_write_the_broker_answered_in_a_way_we_cannot_read_may_have_been_placed() {
        // Success status, body cut off or not JSON.
        for body in ["", "not json", r#"{"success":tr"#] {
            let body = body.to_owned();
            let server = MockServer::start(move |_| Reply::json(200, body.clone()));
            let error = client(&server)
                .post("/orders", &serde_json::json!({}))
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::OutcomeUnknown, "{error:?}");
            assert_eq!(server.request_count(), 1);
        }
        // A redirect or a status nobody listed: the request was delivered.
        for status in [302_u16, 307, 418, 402] {
            let server = MockServer::start(move |_| {
                Reply::json(status, "{}").with_header("Location", "http://127.0.0.1:1/elsewhere")
            });
            let error = client(&server)
                .post("/orders", &serde_json::json!({}))
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::OutcomeUnknown, "HTTP {status}");
            assert_eq!(server.request_count(), 1, "HTTP {status}");
        }
        // Reads keep their own answers: nothing was changed, so nothing is unknown.
        let server = MockServer::start(|_| Reply::json(200, "not json"));
        let error = client(&server).get("/x", &[]).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Invalid);
        let server = MockServer::start(|_| Reply::json(200, "not json"));
        let error = client(&server)
            .post_read("/orders/preview", &serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Invalid);
    }

    #[test]
    fn a_post_carries_its_json_body() {
        let server = MockServer::start(|_| Reply::json(200, "{}"));
        client(&server)
            .post("/orders", &serde_json::json!({"client_order_id": "abc"}))
            .unwrap();
        let seen = &server.requests()[0];
        assert_eq!(seen.method, "POST");
        assert_eq!(seen.header("content-type"), Some("application/json"));
        assert!(seen.body.contains("\"client_order_id\":\"abc\""));
    }

    #[test]
    fn redirects_are_not_followed() {
        let server = MockServer::start(|_| {
            Reply::json(302, "{}").with_header("Location", "http://127.0.0.1:1/elsewhere")
        });
        let error = client(&server).get("/x", &[]).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Invalid);
        assert_eq!(server.request_count(), 1);
    }

    #[test]
    fn the_limiter_spaces_calls() {
        let limiter = RateLimiter::new(Duration::from_millis(40));
        let start = Instant::now();
        limiter.wait();
        limiter.wait();
        limiter.wait();
        assert!(start.elapsed() >= Duration::from_millis(80));
    }
}
