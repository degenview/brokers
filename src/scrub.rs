//! Removes credentials from text before it is shown, stored or sent anywhere.
//!
//! There is no logging framework behind this crate, and errors cross into an application that may
//! print them. So every string that leaves an adapter as an error detail passes through a
//! [`Scrubber`] built from the credentials in use.

const REDACTED: &str = "[redacted]";

/// Replaces known secret values and anything shaped like a signed token.
#[derive(Clone, Default)]
pub struct Scrubber {
    secrets: Vec<String>,
}

impl Scrubber {
    pub fn new<'a>(values: impl IntoIterator<Item = &'a str>) -> Self {
        let mut secrets: Vec<String> = values
            .into_iter()
            .map(str::trim)
            // A one- or two-character "secret" would shred ordinary text; real credentials are longer.
            .filter(|value| value.len() >= 6)
            .flat_map(variants)
            .collect();
        // Longest first, so a value that contains another is replaced whole.
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        Self { secrets }
    }

    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), REDACTED);
            }
        }
        redact_tokens(&out)
    }
}

/// The forms a value may take once it has been through a log line or a JSON string: as given,
/// with its newlines escaped, and with them removed.
fn variants(value: &str) -> Vec<String> {
    let mut forms = vec![value.to_owned()];
    if value.contains('\n') {
        forms.push(value.replace('\n', "\\n"));
        forms.push(value.replace("\r\n", "").replace('\n', ""));
        // Each line on its own: a PEM body may be reported a line at a time.
        for line in value.lines().map(str::trim).filter(|line| line.len() >= 16) {
            forms.push(line.to_owned());
        }
    }
    forms
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Replaces anything shaped like a JWT (`eyJ…`.`…`.`…`) and `Bearer` values.
fn redact_tokens(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        if rest.starts_with("eyJ")
            && let Some(length) = jwt_length(rest)
        {
            out.push_str(REDACTED);
            index += length;
            continue;
        }
        if rest.len() >= 7 && rest.is_char_boundary(7) && rest[..7].eq_ignore_ascii_case("bearer ")
        {
            out.push_str(&rest[..7]);
            let value: usize = rest[7..]
                .chars()
                .take_while(|c| is_token_char(*c) || *c == '.')
                .map(char::len_utf8)
                .sum();
            if value > 0 {
                out.push_str(REDACTED);
            }
            index += 7 + value;
            continue;
        }
        let c = rest.chars().next().unwrap_or('\0');
        out.push(c);
        index += c.len_utf8();
        let _ = bytes;
    }
    out
}

/// The length of a `header.payload.signature` token at the start of `text`, if it is one.
fn jwt_length(text: &str) -> Option<usize> {
    let mut length = 0;
    let mut dots = 0;
    for c in text.chars() {
        if c == '.' {
            dots += 1;
            if dots > 2 {
                break;
            }
        } else if !is_token_char(c) {
            break;
        }
        length += c.len_utf8();
    }
    (dots == 2 && length > 20).then_some(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_secrets_disappear_from_every_form() {
        let pem = "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIABCDEFGHIJKLMNOP\nQRSTUVWXYZ0123456789==\n-----END EC PRIVATE KEY-----\n";
        let scrubber = Scrubber::new(["organizations/o1/apiKeys/k1", pem]);
        let shown = scrubber.scrub(&format!(
            "bad key organizations/o1/apiKeys/k1; body {}; escaped {}; line MHcCAQEEIABCDEFGHIJKLMNOP",
            pem,
            pem.replace('\n', "\\n")
        ));
        assert!(!shown.contains("organizations"));
        assert!(!shown.contains("MHcCAQEE"));
        assert!(!shown.contains("QRSTUVWXYZ"));
        assert!(shown.contains("[redacted]"));
    }

    #[test]
    fn jwt_shaped_tokens_and_bearer_values_are_removed_without_knowing_them() {
        let scrubber = Scrubber::default();
        let token = "eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJ4In0.c2lnbmF0dXJlLWJ5dGVz";
        let shown = scrubber.scrub(&format!(
            "sent {token} and Authorization: Bearer abc.def-123_xyz done"
        ));
        assert!(!shown.contains("eyJ"));
        assert!(!shown.contains("abc.def"));
        assert!(shown.contains("Bearer [redacted]"));
        assert!(shown.contains("done"));
    }

    #[test]
    fn ordinary_text_and_short_values_are_left_alone() {
        let scrubber = Scrubber::new(["ab", ""]);
        assert_eq!(scrubber.scrub("an ab cd order 123"), "an ab cd order 123");
        assert_eq!(
            scrubber.scrub("INSUFFICIENT_FUND for BTC-USD, héllo ✓"),
            "INSUFFICIENT_FUND for BTC-USD, héllo ✓"
        );
    }
}
