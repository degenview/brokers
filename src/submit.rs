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

use crate::adapter::{ClientIdRules, Connection};
use crate::error::{BrokerError, ErrorKind, Result};
use crate::model::orders::PlacedOrder;
use crate::model::pending::{PendingJournal, PendingSubmission, SubmissionOutcome};
use crate::model::types::{ClientOrderId, LiveOrder, OrderDraft};

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

/// What a reconciliation found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reconciled {
    /// The broker has the order.
    Placed(Box<LiveOrder>),
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
) -> Result<PlacedOrder> {
    let entry = PendingSubmission {
        broker: connection.broker(),
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
) -> Result<PlacedOrder> {
    match connection.place(&entry.draft, &entry.client_id) {
        Ok(placed) => {
            journal
                .resolve(
                    entry.broker,
                    &entry.client_id,
                    &SubmissionOutcome::Placed(placed.order_id.clone()),
                )
                .map_err(journal_failure)?;
            Ok(placed)
        }
        // The broker said no, clearly: nothing is live.
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
    let since = entry.created_ms.saturating_sub(LOOKBACK_MS);
    match connection.find_by_client_id(&entry.client_id, since)? {
        Some(order) => {
            journal
                .resolve(
                    entry.broker,
                    &entry.client_id,
                    &SubmissionOutcome::Placed(order.id.clone()),
                )
                .map_err(journal_failure)?;
            Ok(Reconciled::Placed(Box::new(order)))
        }
        None if now_ms.saturating_sub(entry.created_ms) >= SETTLE_MS => {
            journal
                .resolve(
                    entry.broker,
                    &entry.client_id,
                    &SubmissionOutcome::NotPlaced,
                )
                .map_err(journal_failure)?;
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
) -> Result<PlacedOrder> {
    match reconcile(connection, journal, entry, now_ms)? {
        Reconciled::Placed(order) => return Ok(placed_from(&order)),
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
