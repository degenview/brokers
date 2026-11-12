//! Orders sent but not yet known to have been accepted or refused.
//!
//! An order is written here **before** it is sent. If the connection then dies, the entry is the
//! only record that something may be live at the broker; nothing is retried until it is resolved by
//! a lookup on the [`ClientOrderId`].

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::types::{AccountId, BrokerId, ClientOrderId, OrderDraft, OrderId};
use crate::adapter::ClientIdRules;

/// A submission that has been recorded and not yet resolved.
///
/// It belongs to **one account** at the broker (a portfolio or sub-account): a client order id is
/// only looked up, and an order only re-sent, through a credential for that account. A record with
/// no account (written before records carried one) belongs to no account we can name, so no
/// session may settle, retry or delete it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingSubmission {
    pub broker: BrokerId,
    #[serde(default)]
    pub account: AccountId,
    pub client_id: ClientOrderId,
    pub draft: OrderDraft,
    pub created_ms: i64,
}

impl PendingSubmission {
    /// Whether a session on `account` may look this submission up, settle it or send it again.
    /// Never true for a record with no account, nor for an empty `account`.
    pub fn belongs_to(&self, account: &AccountId) -> bool {
        !self.account.as_str().is_empty() && &self.account == account
    }
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
    /// Closes the record of `client_id` **for `account`**. A record of another account with the
    /// same client id is left alone.
    fn resolve(
        &self,
        broker: BrokerId,
        account: &AccountId,
        client_id: &ClientOrderId,
        outcome: &SubmissionOutcome,
    ) -> Result<(), JournalError>;
    /// Every unresolved record of the broker, **whatever account it belongs to**. The runtime keeps
    /// to its own account's records; the rest are held untouched.
    fn unresolved(&self, broker: BrokerId) -> Result<Vec<PendingSubmission>, JournalError>;
}

/// Where a [`BlobJournal`] keeps its whole list of records: two operations, so an application can
/// back it with one row, one setting or one file.
pub trait JournalStorage: Send + Sync {
    /// Every record kept. An error (the storage is unreachable, or what is there cannot be decoded)
    /// is never "nothing pending": that would forget an order that may be live.
    fn load(&self) -> Result<Vec<PendingSubmission>, JournalError>;
    /// Replaces everything kept with `entries`, durably.
    fn save(&self, entries: &[PendingSubmission]) -> Result<(), JournalError>;
}

/// A [`PendingJournal`] over a [`JournalStorage`]: one writer at a time doing read, change, write.
/// A record is keyed by broker, **account** and client id, so the same client id in two portfolios
/// is two orders. If the list cannot be read nothing is written, so nothing is lost by a bad read.
pub struct BlobJournal<S> {
    storage: S,
    /// One writer at a time: a read-modify-write of the whole list.
    gate: Mutex<()>,
}

impl<S: JournalStorage> BlobJournal<S> {
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            gate: Mutex::new(()),
        }
    }
}

impl<S: JournalStorage> PendingJournal for BlobJournal<S> {
    fn record(&self, entry: &PendingSubmission) -> Result<(), JournalError> {
        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries = self.storage.load()?;
        entries.retain(|e| {
            !(e.broker == entry.broker
                && e.account == entry.account
                && e.client_id == entry.client_id)
        });
        entries.push(entry.clone());
        self.storage.save(&entries)
    }

    fn resolve(
        &self,
        broker: BrokerId,
        account: &AccountId,
        client_id: &ClientOrderId,
        _outcome: &SubmissionOutcome,
    ) -> Result<(), JournalError> {
        let _gate = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        let mut entries = self.storage.load()?;
        let before = entries.len();
        entries.retain(|e| {
            !(e.broker == broker && &e.account == account && &e.client_id == client_id)
        });
        if entries.len() == before {
            return Ok(());
        }
        self.storage.save(&entries)
    }

    fn unresolved(&self, broker: BrokerId) -> Result<Vec<PendingSubmission>, JournalError> {
        Ok(self
            .storage
            .load()?
            .into_iter()
            .filter(|entry| entry.broker == broker)
            .collect())
    }
}

/// A journal in memory, for tests and for sessions that need none.
#[derive(Default)]
pub struct MemoryJournal {
    entries: Mutex<BTreeMap<(String, String, String), PendingSubmission>>,
    /// Test hook: make `record` fail.
    pub fail_to_record: std::sync::atomic::AtomicBool,
    /// Test hook: make `resolve` fail (the entry stays).
    pub fail_to_resolve: std::sync::atomic::AtomicBool,
    /// Test hook: make `unresolved` fail, as a corrupt record would.
    pub fail_to_read: std::sync::atomic::AtomicBool,
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
                (
                    entry.broker.as_str().to_owned(),
                    entry.account.0.clone(),
                    entry.client_id.0.clone(),
                ),
                entry.clone(),
            );
        Ok(())
    }

    fn resolve(
        &self,
        broker: BrokerId,
        account: &AccountId,
        client_id: &ClientOrderId,
        _outcome: &SubmissionOutcome,
    ) -> Result<(), JournalError> {
        if self
            .fail_to_resolve
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(JournalError("the journal is not writable".to_owned()));
        }
        self.entries
            .lock()
            .map_err(|_| JournalError("journal poisoned".to_owned()))?
            .remove(&(
                broker.as_str().to_owned(),
                account.0.clone(),
                client_id.0.clone(),
            ));
        Ok(())
    }

    fn unresolved(&self, broker: BrokerId) -> Result<Vec<PendingSubmission>, JournalError> {
        if self.fail_to_read.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(JournalError("the journal cannot be read".to_owned()));
        }
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

    #[derive(Default)]
    struct Blob {
        kept: Mutex<Vec<PendingSubmission>>,
        unreadable: std::sync::atomic::AtomicBool,
        unwritable: std::sync::atomic::AtomicBool,
        saves: std::sync::atomic::AtomicUsize,
    }

    impl JournalStorage for &Blob {
        fn load(&self) -> Result<Vec<PendingSubmission>, JournalError> {
            if self.unreadable.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(JournalError("cannot be decoded".to_owned()));
            }
            Ok(self.kept.lock().unwrap().clone())
        }

        fn save(&self, entries: &[PendingSubmission]) -> Result<(), JournalError> {
            if self.unwritable.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(JournalError("disk full".to_owned()));
            }
            self.saves.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.kept.lock().unwrap() = entries.to_vec();
            Ok(())
        }
    }

    fn submission(broker: &'static str, account: &str, client: &str) -> PendingSubmission {
        use crate::model::types::{OrderKind, ProductId, Side};
        PendingSubmission {
            broker: BrokerId::new(broker),
            account: AccountId::new(account),
            client_id: ClientOrderId::new(client),
            draft: OrderDraft {
                product: ProductId::new("BTC-USD"),
                side: Side::Buy,
                kind: OrderKind::Market {
                    size: crate::model::types::OrderSize::Base(crate::model::test_support::d("1")),
                },
            },
            created_ms: 0,
        }
    }

    #[test]
    fn a_blob_journal_keys_records_by_account_and_resolves_only_its_own() {
        let blob = Blob::default();
        let journal = BlobJournal::new(&blob);
        let broker = BrokerId::new("coinbase");
        let (a, b) = (AccountId::new("a"), AccountId::new("b"));
        journal.record(&submission("coinbase", "a", "c1")).unwrap();
        journal.record(&submission("coinbase", "b", "c1")).unwrap();
        // Recording the same order again replaces it rather than doubling it.
        journal.record(&submission("coinbase", "a", "c1")).unwrap();
        assert_eq!(journal.unresolved(broker).unwrap().len(), 2);
        let none = SubmissionOutcome::NotPlaced;
        journal
            .resolve(broker, &a, &ClientOrderId::new("c1"), &none)
            .unwrap();
        let left = journal.unresolved(broker).unwrap();
        assert_eq!(
            left.len(),
            1,
            "the other account's order with the same id stays"
        );
        assert_eq!(left[0].account, b);
        // Closing what is not there writes nothing.
        let saves = blob.saves.load(std::sync::atomic::Ordering::SeqCst);
        journal
            .resolve(broker, &a, &ClientOrderId::new("c1"), &none)
            .unwrap();
        assert_eq!(blob.saves.load(std::sync::atomic::Ordering::SeqCst), saves);
    }

    #[test]
    fn a_blob_journal_lists_one_brokers_records_only() {
        let blob = Blob::default();
        let journal = BlobJournal::new(&blob);
        journal.record(&submission("coinbase", "a", "c1")).unwrap();
        journal.record(&submission("other", "a", "c2")).unwrap();
        let found = journal.unresolved(BrokerId::new("other")).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].client_id.as_str(), "c2");
    }

    #[test]
    fn an_unreadable_list_is_an_error_and_is_never_overwritten() {
        let blob = Blob::default();
        let journal = BlobJournal::new(&blob);
        journal.record(&submission("coinbase", "a", "c1")).unwrap();
        blob.unreadable
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let saves = blob.saves.load(std::sync::atomic::Ordering::SeqCst);
        assert!(journal.record(&submission("coinbase", "a", "c2")).is_err());
        assert!(
            journal
                .resolve(
                    BrokerId::new("coinbase"),
                    &AccountId::new("a"),
                    &ClientOrderId::new("c1"),
                    &SubmissionOutcome::NotPlaced
                )
                .is_err()
        );
        assert!(journal.unresolved(BrokerId::new("coinbase")).is_err());
        assert_eq!(
            blob.saves.load(std::sync::atomic::Ordering::SeqCst),
            saves,
            "nothing was written over what could not be read"
        );
    }

    #[test]
    fn a_failed_write_is_reported_so_the_order_is_not_sent() {
        let blob = Blob::default();
        let journal = BlobJournal::new(&blob);
        blob.unwritable
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(journal.record(&submission("coinbase", "a", "c1")).is_err());
    }
}
