//! A venue-neutral trading model and broker adapters for DegenView.
//!
//! The crate has no user interface, no database and no async runtime. It talks to the application
//! through small traits ([`SecretStore`], [`model::pending::PendingJournal`], [`Waker`],
//! [`NoticeSink`] and [`Clock`]), so the application decides where secrets and records live.
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
pub mod net;
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
pub use net::{Direct, Network, Proxy, ProxyCredentials, ProxyProtocol, Route};
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

    /// Every `.rs` file under `dir`, with its text. An adapter's own module (where its name
    /// belongs) is left out unless `with_adapters`.
    fn sources(dir: &Path, with_adapters: bool, out: &mut Vec<(String, String)>) {
        for entry in fs::read_dir(dir).expect("read the source tree") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                if !with_adapters && path.file_name().is_some_and(|name| name == "coinbase") {
                    continue;
                }
                sources(&path, with_adapters, out);
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
            false,
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

    /// Every connection is made through [`crate::net::Network`], so a proxy setting reaches all of
    /// them. Only the network and HTTP layers may open a socket or build an HTTP agent; test
    /// support (the `testing` directory, fake exchanges, everything after `#[cfg(test)]`) may do
    /// what it likes.
    #[test]
    fn connections_are_only_opened_through_the_network_port() {
        const FORBIDDEN: [&str; 5] = [
            "TcpStream::connect",
            "Agent::config_builder",
            "Agent::new_with_defaults",
            "ureq::get(",
            "ureq::post(",
        ];
        let mut files = Vec::new();
        sources(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            true,
            &mut files,
        );
        assert!(files.len() > 10, "the guard must actually read the tree");
        for (path, text) in files {
            let is_test_support = path.contains("/src/testing/")
                || path.ends_with("_tests.rs")
                || path.ends_with("/fake.rs");
            if is_test_support || path.ends_with("/net.rs") || path.ends_with("/http.rs") {
                continue;
            }
            let production = text.split("#[cfg(test)]").next().unwrap_or("");
            for call in FORBIDDEN {
                assert!(
                    !production.contains(call),
                    "{path} uses `{call}`; connections go through `net::Network` (or `http`)"
                );
            }
        }
    }
}
