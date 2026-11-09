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
pub mod error;
pub mod http;
pub mod model;
pub mod registry;
pub mod scrub;
pub mod secret;
pub mod submit;

#[cfg(feature = "coinbase")]
pub mod coinbase;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use adapter::{
    AccountModel, Broker, ClientIdRules, Connection, CredentialField, Descriptor, Env, FieldKind,
    MarketRef, ProductResolution, Session,
};
pub use error::{BrokerError, ErrorKind, Result};
pub use model::capabilities::{Capabilities, Tradability};
pub use model::types::*;
pub use model::validate;
pub use rust_decimal::Decimal;
pub use secret::{Credentials, MemorySecretStore, SecretStore, SecretStoreError};
