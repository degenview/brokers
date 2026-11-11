//! Placing an order without ever placing it twice or losing track of it.
//!
//! The rules, in order:
//!
//! 1. The order is written to the [`PendingJournal`] **before** it is sent. If it cannot be
//!    recorded it is not sent.
//! 2. It is sent exactly once. A write that did not finish ends as `OutcomeUnknown` and the journal
//!    entry stays.
//! 3. Only a lookup by [`ClientOrderId`] resolves an unknown outcome ([`reconcile`]). "Not found"
//!    is believed only after the broker has had [`SETTLE_MS`] to list it.
//! 4. Sending again is an explicit act ([`retry`]), uses the **same** client order id, and looks
//!    first.
//! 5. A submission belongs to the **account** of the session that made it. Only a session on that
//!    account looks it up, settles it or sends it again; a session on another account (a different
//!    portfolio or sub-account, say) refuses, because the same client order id means nothing there and a
//!    "not found" would be a lie that deletes the only record of a live order.
//! 6. "Not sent" is said only when nothing left the machine: the order could not be recorded, or
//!    the broker clearly refused it. Once the broker has accepted an order, failing to *update the
//!    journal* never takes that back: the order is returned as placed, with the failure alongside
//!    ([`Submitted::unsettled`]), and the entry stays until a lookup can settle it.

use crate::adapter::{ClientIdRules, Connection};
use crate::error::{BrokerError, ErrorKind, Result};
use crate::model::orders::PlacedOrder;
use crate::model::pending::{JournalError, PendingJournal, PendingSubmission, SubmissionOutcome};
use crate::model::types::{AccountId, ClientOrderId, LiveOrder, OrderDraft};

/// How long a broker is given to list an order it accepted before "not listed" is believed.
pub const SETTLE_MS: i64 = 120_000;
/// How far before the submission a lookup starts, to allow for clock differences.
const LOOKBACK_MS: i64 = 60_000;

fn journal_failure(error: impl std::fmt::Display) -> BrokerError {
    BrokerError::new(
        ErrorKind::Invalid,
        "The order could not be recorded safely, so it was not sent.",
    )
    .with_detail(error.to_string())
}

/// The account this connection trades in. A connection that cannot name one cannot send an order:
/// the record could not say whose it is.
pub fn session_account(connection: &dyn Connection) -> Result<AccountId> {
    connection
        .session()
        .accounts
        .first()
        .map(|account| account.id.clone())
        .filter(|id| !id.as_str().is_empty())
        .ok_or_else(|| {
            BrokerError::new(
                ErrorKind::Invalid,
                "The connection does not name an account, so the order was not sent.",
            )
        })
}

fn other_account() -> BrokerError {
    BrokerError::new(
        ErrorKind::Invalid,
        "That order belongs to another account. Nothing was sent or changed.",
    )
}

/// The session's account, if `entry` is its own.
fn own_account(connection: &dyn Connection, entry: &PendingSubmission) -> Result<AccountId> {
    let account = session_account(connection)?;
    if entry.belongs_to(&account) {
        Ok(account)
    } else {
        Err(other_account())
    }
}

/// An order the broker accepted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Submitted {
    pub placed: PlacedOrder,
    /// The journal entry could not be marked settled. The order is live all the same; the entry
    /// stays, and a later lookup settles it.
    pub unsettled: Option<JournalError>,
}

/// What a reconciliation found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reconciled {
    /// The broker has the order.
    Placed(Box<LiveOrder>),
    /// The broker has the order, and the journal entry could not be marked settled.
    PlacedUnsettled(Box<LiveOrder>, JournalError),
    /// The broker does not list it and has had long enough: it was not placed.
    NotPlaced,
    /// The broker does not list it yet. Ask again shortly; do not resend.
    NotListedYet,
}

fn placed_from(order: &LiveOrder) -> PlacedOrder {
    PlacedOrder {
        order_id: order.id.clone(),
        product: order.product.clone(),
        side: order.side,
    }
}

/// Records, sends and settles one order. See the module rules.
pub fn submit(
    connection: &dyn Connection,
    journal: &dyn PendingJournal,
    draft: &OrderDraft,
    client_id: &ClientOrderId,
    now_ms: i64,
) -> Result<Submitted> {
    let entry = PendingSubmission {
        broker: connection.broker(),
        account: session_account(connection)?,
        client_id: client_id.clone(),
        draft: draft.clone(),
        created_ms: now_ms,
    };
    journal.record(&entry).map_err(journal_failure)?;
    send(connection, journal, &entry)
}

fn send(
    connection: &dyn Connection,
    journal: &dyn PendingJournal,
    entry: &PendingSubmission,
) -> Result<Submitted> {
    match connection.place(&entry.draft, &entry.client_id) {
        Ok(placed) => {
            // The broker has the order. Whatever happens to the journal, it is live.
            let unsettled = journal
                .resolve(
                    entry.broker,
                    &entry.account,
                    &entry.client_id,
                    &SubmissionOutcome::Placed(placed.order_id.clone()),
                )
                .err();
            Ok(Submitted { placed, unsettled })
        }
        // The broker said no, clearly, or nothing was sent: nothing is live. (`Invalid` from a
        // write means no request left: the HTTP layer reports every unreadable answer as
        // `OutcomeUnknown`.)
        Err(error)
            if matches!(
                error.kind,
                ErrorKind::Rejected
                    | ErrorKind::Invalid
                    | ErrorKind::Unsupported
                    | ErrorKind::Auth
                    | ErrorKind::Permission
            ) =>
        {
            let _ = journal.resolve(
                entry.broker,
                &entry.account,
                &entry.client_id,
                &SubmissionOutcome::NotPlaced,
            );
            Err(error)
        }
        // Anything else may have reached the broker: the entry stays until a lookup settles it.
        Err(error) => Err(BrokerError {
            kind: ErrorKind::OutcomeUnknown,
            ..error
        }),
    }
}

/// Looks the order up by client id and settles the journal entry if the answer is final.
pub fn reconcile(
    connection: &dyn Connection,
    journal: &dyn PendingJournal,
    entry: &PendingSubmission,
    now_ms: i64,
) -> Result<Reconciled> {
    own_account(connection, entry)?;
    let since = entry.created_ms.saturating_sub(LOOKBACK_MS);
    match connection.find_by_client_id(&entry.client_id, since)? {
        Some(order) => {
            // Found: it is live whether or not the journal can be told.
            match journal.resolve(
                entry.broker,
                &entry.account,
                &entry.client_id,
                &SubmissionOutcome::Placed(order.id.clone()),
            ) {
                Ok(()) => Ok(Reconciled::Placed(Box::new(order))),
                Err(error) => Ok(Reconciled::PlacedUnsettled(Box::new(order), error)),
            }
        }
        None if now_ms.saturating_sub(entry.created_ms) >= SETTLE_MS => {
            journal
                .resolve(
                    entry.broker,
                    &entry.account,
                    &entry.client_id,
                    &SubmissionOutcome::NotPlaced,
                )
                .map_err(|error| {
                    // The broker does not have it; the entry stays and is looked at again.
                    BrokerError::new(
                        ErrorKind::Transient,
                        "The record of this order could not be updated. It will be checked again.",
                    )
                    .with_detail(error.to_string())
                })?;
            Ok(Reconciled::NotPlaced)
        }
        None => Ok(Reconciled::NotListedYet),
    }
}

/// Sends an unresolved submission again, at the user's request, under its original client order
/// id. It looks first, so an order that was in fact placed is found rather than doubled.
pub fn retry(
    connection: &dyn Connection,
    journal: &dyn PendingJournal,
    rules: &ClientIdRules,
    entry: &PendingSubmission,
    now_ms: i64,
) -> Result<Submitted> {
    own_account(connection, entry)?;
    match reconcile(connection, journal, entry, now_ms)? {
        Reconciled::Placed(order) => {
            return Ok(Submitted {
                placed: placed_from(&order),
                unsettled: None,
            });
        }
        Reconciled::PlacedUnsettled(order, error) => {
            return Ok(Submitted {
                placed: placed_from(&order),
                unsettled: Some(error),
            });
        }
        Reconciled::NotListedYet if !rules.idempotent => {
            return Err(BrokerError::new(
                ErrorKind::OutcomeUnknown,
                "The order is not listed yet. Wait a little and check again before sending it again.",
            ));
        }
        Reconciled::NotListedYet | Reconciled::NotPlaced => {}
    }
    // The lookup settled the entry as not placed; record it again for the new attempt.
    journal.record(entry).map_err(journal_failure)?;
    send(connection, journal, entry)
}
