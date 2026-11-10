//! Request signing. Each broker authenticates differently (a signed token, an HMAC over the
//! query, an HMAC with a nonce); an adapter owns the signer it needs and the rest of the crate
//! only sees [`RequestSigner`]. Signers specific to one broker live in that broker's module.

use crate::error::Result;

pub mod der;

/// The parts of a request a signature may cover.
#[derive(Clone, Copy, Debug)]
pub struct RequestParts<'a> {
    pub method: &'a str,
    pub host: &'a str,
    /// The path without a query string.
    pub path: &'a str,
}

/// Produces the headers that authenticate one request. Called once per request: tokens expire in
/// minutes and are never reused.
pub trait RequestSigner: Send + Sync {
    fn headers(&self, request: &RequestParts<'_>) -> Result<Vec<(String, String)>>;
}
