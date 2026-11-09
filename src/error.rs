//! One error type for every broker. An adapter turns its wire errors into this; nothing above an
//! adapter ever sees raw JSON, a status line or a header.

use std::fmt;

/// What went wrong, in the terms a caller acts on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// The credentials are malformed, wrong, expired or revoked.
    Auth,
    /// The credentials are valid but lack the permission this call needs.
    Permission,
    /// The broker asked us to slow down. `retry_after_ms` is its hint, when it gave one.
    RateLimited { retry_after_ms: Option<u64> },
    /// The broker understood the request and refused it (insufficient funds, bad increment,
    /// market closed...). Retrying the same request will not help.
    Rejected,
    /// A network failure or a 5xx on a call that is safe to repeat.
    Transient,
    /// A call that may have changed state (placing an order) did not finish: the broker may or may
    /// not have acted. The caller must look the order up before doing anything else.
    OutcomeUnknown,
    /// The request is invalid on our side (a draft that fails validation, a malformed value).
    Invalid,
    /// The broker or the product cannot do this.
    Unsupported,
    /// The thing asked about does not exist (any more).
    NotFound,
}

/// A broker failure. `Display` is the sentence for a person; `detail` is for diagnostics and has
/// been scrubbed of every credential, token and signed header.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerError {
    pub kind: ErrorKind,
    /// A short, plain-language sentence. Never raw JSON.
    pub user_text: String,
    /// The broker's own code or reason (`INSUFFICIENT_FUND`), when it sent one.
    pub code: Option<String>,
    /// Redacted technical detail (status, endpoint, the broker's message).
    pub detail: String,
}

impl BrokerError {
    pub fn new(kind: ErrorKind, user_text: impl Into<String>) -> Self {
        Self {
            kind,
            user_text: user_text.into(),
            code: None,
            detail: String::new(),
        }
    }

    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn invalid(user_text: impl Into<String>) -> Self {
        Self::new(ErrorKind::Invalid, user_text)
    }

    pub fn unsupported(user_text: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, user_text)
    }

    /// Whether repeating the identical call could succeed without risk. Only reads are ever
    /// repeated automatically; this says whether a failed read is worth another try.
    pub fn is_transient(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Transient | ErrorKind::RateLimited { .. }
        )
    }
}

impl fmt::Display for BrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.user_text)
    }
}

impl std::error::Error for BrokerError {}

pub type Result<T, E = BrokerError> = std::result::Result<T, E>;
