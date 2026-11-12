//! A venue-neutral trading model and broker adapters for DegenView.
//!
//! The crate has no user interface, no database and no async runtime. It talks to the application
//! through small traits ([`SecretStore`], and in later versions a journal, an event sink and a
//! clock), so the application decides where secrets and records live.
//!
//! * [`model`] is pure: decimal types, order drafts, validation, capabilities.
//! * [`Broker`] / [`Connection`] are what an exchange implements; [`registry`] lists them.
//! * `coinbase` (feature `coinbase`) is the first adapter: Coinbase Advanced Trade, spot.
//!
//! Rules every adapter keeps: no credential is ever logged, formatted or returned in an error;
//! an order is never rounded silently; nothing here can move funds off the exchange.

pub mod adapter;
pub mod auth;
pub mod edit_session;
pub mod error;
pub mod http;
pub mod model;
pub mod registry;
pub mod runtime;
pub mod scrub;
pub mod secret;
pub mod submit;
pub mod ws;

#[cfg(feature = "coinbase")]
pub mod coinbase;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use adapter::{
    AccountModel, Broker, ClientIdRules, Clock, Connection, CredentialField, Descriptor, Env,
    FieldKind, MarketRef, ProductResolution, Session, StreamHandle, StreamTiming, SystemClock,
};
pub use error::{BrokerError, ErrorKind, Result};
pub use model::capabilities::{Capabilities, Tradability};
pub use model::types::*;
pub use model::validate;
pub use runtime::{
    BrokerRuntime, BrokerSnapshot, NoticeSink, OrderBlock, Pending, PendingState, PendingView,
    RuntimeConfig, Status, Waker,
};
pub use rust_decimal::Decimal;
pub use secret::{Credentials, MemorySecretStore, SecretStore, SecretStoreError};

#[cfg(test)]
mod guard_tests {
    use std::fs;
    use std::path::Path;

    fn sources(dir: &Path, out: &mut Vec<(String, String)>) {
        for entry in fs::read_dir(dir).expect("read the source tree") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                // An adapter's own module is where its name belongs.
                if path.file_name().is_some_and(|name| name == "coinbase") {
                    continue;
                }
                sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = fs::read_to_string(&path).expect("read a source file");
                // Forward slashes on every platform, so the comparisons below hold on Windows too.
                out.push((path.display().to_string().replace('\\', "/"), text));
            }
        }
    }

    /// The model, the traits, the HTTP and WebSocket layers, the runtime and the test support know
    /// no exchange by name: adding one must not touch them. Only the crate root and the registry,
    /// which list the adapters, may.
    #[test]
    fn generic_modules_name_no_exchange() {
        let mut files = Vec::new();
        sources(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        assert!(files.len() > 10, "the guard must actually read the tree");
        for (path, text) in files {
            if path.ends_with("src/lib.rs") || path.ends_with("src/registry.rs") {
                continue;
            }
            let lower = text.to_lowercase();
            for name in ["coinbase", "binance", "kraken"] {
                assert!(
                    !lower.contains(name),
                    "{path} names `{name}`; broker-specific code belongs in that broker's module"
                );
            }
        }
    }
}
