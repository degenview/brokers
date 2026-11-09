//! Orders sent but not yet known to have been accepted or refused.
//!
//! An order is written here **before** it is sent. If the connection then dies, the entry is the
//! only record that something may be live at the broker; nothing is retried until it is resolved by
//! a lookup on the [`ClientOrderId`].

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::types::{BrokerId, ClientOrderId, OrderDraft, OrderId};
use crate::adapter::ClientIdRules;

/// A submission that has been recorded and not yet resolved.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingSubmission {
    pub broker: BrokerId,
    pub client_id: ClientOrderId,
    pub draft: OrderDraft,
    pub created_ms: i64,
}

/// How a submission ended.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SubmissionOutcome {
    /// The broker accepted it under this id.
    Placed(OrderId),
    /// The broker refused it, or it was found not to exist after a full lookup.
    NotPlaced,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JournalError(pub String);

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JournalError {}

/// Where an application keeps unresolved submissions. Must survive a restart.
pub trait PendingJournal: Send + Sync {
    /// Durably records `entry`. An error means the order **must not be sent**.
    fn record(&self, entry: &PendingSubmission) -> Result<(), JournalError>;
    fn resolve(
        &self,
        broker: BrokerId,
        client_id: &ClientOrderId,
        outcome: &SubmissionOutcome,
    ) -> Result<(), JournalError>;
    fn unresolved(&self, broker: BrokerId) -> Result<Vec<PendingSubmission>, JournalError>;
}

/// A journal in memory, for tests and for sessions that need none.
#[derive(Default)]
pub struct MemoryJournal {
    entries: Mutex<BTreeMap<(String, String), PendingSubmission>>,
    /// Test hook: make `record` fail.
    pub fail_to_record: std::sync::atomic::AtomicBool,
}

impl MemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PendingJournal for MemoryJournal {
    fn record(&self, entry: &PendingSubmission) -> Result<(), JournalError> {
        if self
            .fail_to_record
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(JournalError("the journal is not writable".to_owned()));
        }
        self.entries
            .lock()
            .map_err(|_| JournalError("journal poisoned".to_owned()))?
            .insert(
                (entry.broker.as_str().to_owned(), entry.client_id.0.clone()),
                entry.clone(),
            );
        Ok(())
    }

    fn resolve(
        &self,
        broker: BrokerId,
        client_id: &ClientOrderId,
        _outcome: &SubmissionOutcome,
    ) -> Result<(), JournalError> {
        self.entries
            .lock()
            .map_err(|_| JournalError("journal poisoned".to_owned()))?
            .remove(&(broker.as_str().to_owned(), client_id.0.clone()));
        Ok(())
    }

    fn unresolved(&self, broker: BrokerId) -> Result<Vec<PendingSubmission>, JournalError> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| JournalError("journal poisoned".to_owned()))?
            .values()
            .filter(|entry| entry.broker == broker)
            .cloned()
            .collect())
    }
}

/// A fresh id for an order, in the shape of a random UUID (36 characters, hex and dashes), which
/// fits every broker's rules we know of. `None` if the broker's maximum is shorter than that.
pub fn new_client_order_id(rules: &ClientIdRules) -> Option<ClientOrderId> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 1
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let id = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    );
    (id.len() <= rules.max_len).then(|| ClientOrderId::new(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ids_are_uuid_shaped_unique_and_respect_the_length_limit() {
        let rules = ClientIdRules {
            max_len: 128,
            idempotent: true,
        };
        let a = new_client_order_id(&rules).unwrap();
        let b = new_client_order_id(&rules).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 36);
        assert_eq!(a.as_str().as_bytes()[14], b'4', "version nibble");
        assert!(matches!(
            a.as_str().as_bytes()[19],
            b'8' | b'9' | b'a' | b'b'
        ));
        assert!(
            new_client_order_id(&ClientIdRules {
                max_len: 20,
                idempotent: true
            })
            .is_none()
        );
    }
}
