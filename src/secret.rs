//! Credentials and where they are kept.
//!
//! The crate never decides where a secret lives. The application implements [`SecretStore`] over
//! the operating system's secret store (Keychain, Credential Manager, Secret Service); this module
//! only gives the value a shape that cannot leak by accident: no `Debug` output, no `Display`,
//! overwritten when dropped.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::model::types::BrokerId;

/// The values a broker's credential form collects, keyed by the descriptor's field keys.
#[derive(Clone, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Credentials {
    values: BTreeMap<String, String>,
}

impl Credentials {
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<String>) -> Self {
        self.values.insert(key.to_owned(), value.into());
        self
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.values.insert(key.to_owned(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.values.values().all(|value| value.trim().is_empty())
    }

    /// Every non-empty value, for building a [`crate::scrub::Scrubber`].
    pub fn secret_values(&self) -> impl Iterator<Item = &str> {
        self.values
            .values()
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
    }

    /// The stored form: a JSON object. Handled only by a [`SecretStore`] implementation.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

impl fmt::Debug for Credentials {
    /// Names the fields and nothing else.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys: Vec<&String> = self.values.keys().collect();
        write!(f, "Credentials{{fields: {keys:?}, values: <redacted>}}")
    }
}

impl Drop for Credentials {
    fn drop(&mut self) {
        // Best effort: overwrite before the allocation is released. Not a guarantee against a
        // memory dump, only against a stale copy sitting in freed memory.
        for value in self.values.values_mut() {
            // SAFETY-free overwrite: replace each byte through a safe API.
            let length = value.len();
            value.clear();
            value.extend(std::iter::repeat_n('\0', length));
            value.clear();
        }
    }
}

/// A failure to read or write the secret store. Carries no secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretStoreError(pub String);

impl fmt::Display for SecretStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SecretStoreError {}

/// Where an application keeps broker credentials. One entry per `(broker, profile)`; the profile
/// is `"default"` until a broker supports several keys at once.
pub trait SecretStore: Send + Sync {
    fn load(
        &self,
        broker: BrokerId,
        profile: &str,
    ) -> Result<Option<Credentials>, SecretStoreError>;
    fn save(
        &self,
        broker: BrokerId,
        profile: &str,
        credentials: &Credentials,
    ) -> Result<(), SecretStoreError>;
    fn delete(&self, broker: BrokerId, profile: &str) -> Result<(), SecretStoreError>;

    /// Whether credentials are stored, without reading them (so no prompt where the platform would
    /// ask). The default reads them.
    fn is_configured(&self, broker: BrokerId, profile: &str) -> bool {
        matches!(self.load(broker, profile), Ok(Some(_)))
    }
}

/// An in-memory store for tests and for sessions that must not touch the platform.
#[derive(Default)]
pub struct MemorySecretStore {
    entries: Mutex<BTreeMap<(String, String), Credentials>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for MemorySecretStore {
    fn load(
        &self,
        broker: BrokerId,
        profile: &str,
    ) -> Result<Option<Credentials>, SecretStoreError> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| SecretStoreError("secret store poisoned".to_owned()))?;
        Ok(entries
            .get(&(broker.as_str().to_owned(), profile.to_owned()))
            .cloned())
    }

    fn save(
        &self,
        broker: BrokerId,
        profile: &str,
        credentials: &Credentials,
    ) -> Result<(), SecretStoreError> {
        self.entries
            .lock()
            .map_err(|_| SecretStoreError("secret store poisoned".to_owned()))?
            .insert(
                (broker.as_str().to_owned(), profile.to_owned()),
                credentials.clone(),
            );
        Ok(())
    }

    fn delete(&self, broker: BrokerId, profile: &str) -> Result<(), SecretStoreError> {
        self.entries
            .lock()
            .map_err(|_| SecretStoreError("secret store poisoned".to_owned()))?
            .remove(&(broker.as_str().to_owned(), profile.to_owned()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_a_value() {
        let credentials = Credentials::new()
            .with("name", "organizations/abc/apiKeys/def")
            .with("private_key", "-----BEGIN EC PRIVATE KEY-----\nSECRET\n");
        let shown = format!("{credentials:?}");
        assert!(!shown.contains("organizations"));
        assert!(!shown.contains("SECRET"));
        assert!(shown.contains("private_key"));
    }

    #[test]
    fn credentials_round_trip_through_their_stored_form() {
        let credentials = Credentials::new().with("a", "1").with("b", "2");
        let again = Credentials::from_json(&credentials.to_json()).unwrap();
        assert_eq!(again.get("a"), Some("1"));
        assert_eq!(again.get("b"), Some("2"));
        assert!(Credentials::from_json("not json").is_none());
    }

    #[test]
    fn blank_values_do_not_count_as_credentials() {
        assert!(Credentials::new().is_empty());
        assert!(Credentials::new().with("a", "  ").is_empty());
        assert!(!Credentials::new().with("a", "x").is_empty());
        assert_eq!(Credentials::new().with("a", "").secret_values().count(), 0);
    }
}
