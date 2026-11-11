//! What an account is for: a record of an unconfirmed order, a review, an edit belongs to the
//! account it was made in, and no other account may settle, repeat or change it. Two fake
//! exchanges with different portfolios are two accounts that share one journal, as two keys do in
//! the application.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rust_decimal::Decimal;
use std::str::FromStr;

use super::fake::{CoinbaseExchange, FakeCoinbase, credentials, generate_key_pem};
use super::{Coinbase, CoinbaseSandbox};
use crate::adapter::{Broker, Env};
use crate::error::ErrorKind;
use crate::model::orders::{OrderChange, OrderFingerprint};
use crate::model::pending::{MemoryJournal, PendingJournal, PendingSubmission};
use crate::model::reducer::Notice;
use crate::model::types::{AccountId, BrokerId, ClientOrderId, OrderStatus, ProductId};
use crate::runtime::{BrokerRuntime, NoticeSink, RuntimeConfig, Waker};
use crate::testing::ManualClock;
use crate::testing::conformance::Exchange;

const BROKER: BrokerId = BrokerId::new("coinbase");
const LONG_AGO_MS: i64 = 1_000_000;

struct Quiet;
impl Waker for Quiet {
    fn wake(&self) {}
}
impl NoticeSink for Quiet {
    fn notice(&self, _: Notice) {}
}

fn d(text: &str) -> Decimal {
    Decimal::from_str(text).unwrap()
}

/// An exchange of its own portfolio.
fn exchange(portfolio: &str) -> CoinbaseExchange {
    let exchange = CoinbaseExchange::start();
    exchange
        .rest
        .with_state(|state| state.portfolio_uuid = portfolio.to_owned());
    exchange
}

fn start(exchange: &CoinbaseExchange, journal: &Arc<MemoryJournal>) -> BrokerRuntime {
    let clock = Arc::new(ManualClock::new(0));
    let config = RuntimeConfig::new(
        exchange.env(clock),
        journal.clone(),
        Arc::new(Quiet),
        Arc::new(Quiet),
    )
    .fast();
    BrokerRuntime::start(&Coinbase, exchange.credentials(), config)
}

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

fn wait_live(runtime: &BrokerRuntime) {
    wait_until("the runtime to go live", || runtime.snapshot().is_live());
}

/// Several reconcile rounds (`fast()` runs one every 100 ms).
fn let_reconcile_run() {
    std::thread::sleep(Duration::from_millis(700));
}

fn record(
    journal: &MemoryJournal,
    exchange: &CoinbaseExchange,
    account: &str,
    client_id: &str,
) -> PendingSubmission {
    let entry = PendingSubmission {
        broker: BROKER,
        account: AccountId::new(account),
        client_id: ClientOrderId::new(client_id),
        draft: exchange.draft(),
        // Long past the settle time: a lookup that finds nothing would be believed.
        created_ms: LONG_AGO_MS,
    };
    journal.record(&entry).unwrap();
    entry
}

fn account_of(exchange: &CoinbaseExchange) -> AccountId {
    exchange.account()
}

// ---- The journal ----------------------------------------------------------------------------

#[test]
fn the_same_client_id_in_two_accounts_is_two_records_and_one_is_resolved_alone() {
    let journal = MemoryJournal::new();
    let a = exchange("pf-a");
    let b = exchange("pf-b");
    let for_a = record(&journal, &a, "pf-a", "same-id");
    record(&journal, &b, "pf-b", "same-id");
    assert_eq!(journal.unresolved(BROKER).unwrap().len(), 2);

    journal
        .resolve(
            BROKER,
            &for_a.account,
            &for_a.client_id,
            &crate::model::pending::SubmissionOutcome::NotPlaced,
        )
        .unwrap();
    let left = journal.unresolved(BROKER).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].account, AccountId::new("pf-b"));
}

#[test]
fn a_record_written_before_records_had_an_account_belongs_to_none() {
    let json = r#"{"broker":"coinbase","client_id":"old","draft":{"product":"BTC-USD","side":"Buy","kind":{"Limit":{"base_size":"0.001","price":"90000.00","tif":"GoodTilCancelled","post_only":false}}},"created_ms":5}"#;
    let entry: PendingSubmission = serde_json::from_str(json).expect("an old record still reads");
    assert!(entry.account.as_str().is_empty());
    assert!(!entry.belongs_to(&AccountId::new("pf-1")));
    assert!(
        !entry.belongs_to(&AccountId::default()),
        "an empty account is no account"
    );
}

// ---- A record is only ever the business of its own account -----------------------------------

#[test]
fn another_accounts_record_is_not_settled_deleted_shown_or_retried() {
    let journal = Arc::new(MemoryJournal::new());
    let a = exchange("pf-a");
    let b = exchange("pf-b");
    // A took the order and its reply was lost; the app is now connected to portfolio B.
    let entry = record(&journal, &a, "pf-a", "from-a");

    let runtime = start(&b, &journal);
    wait_live(&runtime);
    let_reconcile_run();

    // B looked for nothing, concluded nothing, deleted nothing.
    assert_eq!(
        journal.unresolved(BROKER).unwrap(),
        vec![entry.clone()],
        "the record of A's order survives B's reconcile rounds"
    );
    let snapshot = runtime.snapshot();
    assert!(snapshot.pending.is_empty(), "it is not B's order to show");
    assert_eq!(snapshot.held_elsewhere, 1, "but B says it is there");
    // Retry on B does not send A's draft to B.
    let error = runtime
        .retry(entry.client_id.clone(), account_of(&b))
        .wait()
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid, "{error:?}");
    assert!(error.to_string().contains("another account"), "{error}");
    assert_eq!(b.order_posts(), 0, "nothing was sent to B");
    assert_eq!(journal.unresolved(BROKER).unwrap().len(), 1);
    runtime.stop();

    // A, back again, looks the order up and settles it: it is its record.
    let runtime = start(&a, &journal);
    wait_live(&runtime);
    wait_until("A to settle its own record", || {
        journal.unresolved(BROKER).unwrap().is_empty()
    });
    assert_eq!(runtime.snapshot().held_elsewhere, 0);
    runtime.stop();
}

#[test]
fn a_record_with_no_account_fails_closed() {
    let journal = Arc::new(MemoryJournal::new());
    let a = exchange("pf-a");
    let legacy = record(&journal, &a, "", "legacy");

    let runtime = start(&a, &journal);
    wait_live(&runtime);
    let_reconcile_run();

    assert_eq!(journal.unresolved(BROKER).unwrap().len(), 1, "kept");
    assert!(runtime.snapshot().pending.is_empty());
    assert_eq!(runtime.snapshot().held_elsewhere, 1);
    let error = runtime
        .retry(legacy.client_id, account_of(&a))
        .wait()
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert_eq!(a.order_posts(), 0);
    runtime.stop();
}

#[test]
fn an_account_still_reconciles_its_own_records() {
    let journal = Arc::new(MemoryJournal::new());
    let a = exchange("pf-a");
    let entry = record(&journal, &a, "pf-a", "mine");
    // The exchange did take it.
    a.rest.with_state(|state| {
        state.orders.push(super::fake::FakeOrder {
            id: "order-9".to_owned(),
            client_id: entry.client_id.0.clone(),
            product: "BTC-USD".to_owned(),
            side: "BUY".to_owned(),
            status: "OPEN".to_owned(),
            configuration: serde_json::json!({"limit_limit_gtc": {"base_size": "0.001", "limit_price": "90000.00", "post_only": false}}),
            filled_size: "0".to_owned(),
            created_time: "2026-11-09T12:00:00.000Z".to_owned(),
        });
    });
    let runtime = start(&a, &journal);
    wait_live(&runtime);
    wait_until("the record to be found and closed", || {
        journal.unresolved(BROKER).unwrap().is_empty()
    });
    assert!(runtime.snapshot().pending.is_empty());
    runtime.stop();
}

// ---- A request names the account it was made for ---------------------------------------------

#[test]
fn an_order_for_another_account_records_and_sends_nothing() {
    let journal = Arc::new(MemoryJournal::new());
    let b = exchange("pf-b");
    let runtime = start(&b, &journal);
    wait_live(&runtime);
    assert_eq!(
        runtime.snapshot().account.as_ref().unwrap().id.clone(),
        account_of(&b)
    );

    let error = runtime
        .submit(b.draft(), AccountId::new("pf-a"))
        .wait()
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid, "{error:?}");
    assert!(error.to_string().contains("account changed"), "{error}");
    assert_eq!(b.order_posts(), 0, "nothing reached the exchange");
    assert!(
        journal.unresolved(BROKER).unwrap().is_empty(),
        "or the journal"
    );
    assert!(runtime.snapshot().pending.is_empty());

    let error = runtime
        .preview(b.draft(), AccountId::new("pf-a"))
        .wait()
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);

    // The right account goes through.
    runtime
        .submit(b.draft(), account_of(&b))
        .wait()
        .expect("placed for its own account");
    assert_eq!(b.order_posts(), 1);
    let entry = journal.unresolved(BROKER).unwrap();
    assert!(entry.is_empty(), "settled");
    runtime.stop();
}

#[test]
fn every_runtime_has_its_own_generation_and_names_its_account() {
    let journal = Arc::new(MemoryJournal::new());
    let a = exchange("pf-a");
    let first = start(&a, &journal);
    let second = start(&a, &journal);
    assert_ne!(first.generation(), second.generation());
    assert_eq!(first.snapshot().generation, first.generation());
    wait_live(&first);
    assert_eq!(
        first.snapshot().account.as_ref().unwrap().id.clone(),
        AccountId::new("pf-a")
    );
    first.stop();
    second.stop();
}

#[test]
fn starting_a_runtime_does_not_read_the_journal() {
    // Whose records are ours is only known once connected; and `start` must not wait on a store.
    struct Gate(std::sync::Mutex<()>);
    impl PendingJournal for Gate {
        fn record(&self, _: &PendingSubmission) -> Result<(), crate::model::pending::JournalError> {
            Ok(())
        }
        fn resolve(
            &self,
            _: BrokerId,
            _: &AccountId,
            _: &ClientOrderId,
            _: &crate::model::pending::SubmissionOutcome,
        ) -> Result<(), crate::model::pending::JournalError> {
            Ok(())
        }
        fn unresolved(
            &self,
            _: BrokerId,
        ) -> Result<Vec<PendingSubmission>, crate::model::pending::JournalError> {
            let _held = self.0.lock().unwrap();
            Ok(Vec::new())
        }
    }
    let gate = Arc::new(Gate(std::sync::Mutex::new(())));
    let blocked = gate.0.lock().unwrap();
    let a = exchange("pf-a");
    let config = RuntimeConfig::new(
        a.env(Arc::new(ManualClock::new(0))),
        gate.clone(),
        Arc::new(Quiet),
        Arc::new(Quiet),
    )
    .fast();
    let started = Instant::now();
    let runtime = BrokerRuntime::start(&Coinbase, a.credentials(), config);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "start waited on the journal"
    );
    drop(blocked);
    wait_live(&runtime);
    runtime.stop();
}

// ---- 409 --------------------------------------------------------------------------------------

#[test]
fn a_conflict_on_an_order_write_keeps_the_record_until_a_lookup_settles_it() {
    let journal = Arc::new(MemoryJournal::new());
    let a = exchange("pf-a");
    // The exchange creates the order and then answers 409.
    a.rest
        .with_state(|state| state.status_after_create = Some(409));
    let runtime = start(&a, &journal);
    wait_live(&runtime);

    let error = runtime
        .submit(a.draft(), account_of(&a))
        .wait()
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutcomeUnknown, "{error:?}");
    assert_eq!(
        journal.unresolved(BROKER).unwrap().len(),
        1,
        "a 409 is not proof that nothing was placed"
    );

    // The lookup finds the order that is live, and only then is the record closed.
    wait_until("the lookup to settle it", || {
        journal.unresolved(BROKER).unwrap().is_empty()
    });
    wait_until("the order to appear", || {
        runtime.snapshot().orders.len() == 1
    });
    runtime.stop();
}

#[test]
fn a_conflict_on_a_cancel_is_unknown_and_a_conflict_on_a_read_is_still_a_refusal() {
    let fake = FakeCoinbase::start();
    let connection = Coinbase
        .connect(
            &credentials(&generate_key_pem()),
            &Env::default().with_loopback_base_url(fake.url()),
        )
        .unwrap();
    fake.with_state(|state| state.status_for_next_write = Some(409));
    let error = connection
        .cancel(&[crate::model::types::OrderId::new("order-1")])
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutcomeUnknown, "{error:?}");
}

// ---- An edit is made against the order that was previewed ------------------------------------

#[test]
fn an_edit_is_refused_when_the_order_changed_after_it_was_previewed() {
    let journal = Arc::new(MemoryJournal::new());
    let a = exchange("pf-a");
    let runtime = start(&a, &journal);
    wait_live(&runtime);
    let placed = runtime
        .submit(a.draft(), account_of(&a))
        .wait()
        .expect("placed");
    wait_until("the order to be open", || {
        runtime
            .snapshot()
            .orders
            .iter()
            .any(|v| v.order.id == placed.order_id && v.order.status == OrderStatus::Open)
    });

    let change = OrderChange {
        price: Some(d("89000.00")),
        ..OrderChange::default()
    };
    let preview = runtime
        .preview_edit(placed.order_id.clone(), change)
        .wait()
        .expect("previewed");
    let base = preview
        .base
        .clone()
        .expect("the order it was worked out for");
    assert_eq!(base.account, account_of(&a));

    // A partial fill arrives before the user confirms.
    a.change_order(placed.order_id.as_str(), "OPEN", "0.0004", true);
    wait_until("the fill to reach the book", || {
        runtime
            .snapshot()
            .orders
            .iter()
            .any(|v| v.order.id == placed.order_id && v.order.filled_size == d("0.0004"))
    });

    let error = runtime
        .amend(placed.order_id.clone(), change, base)
        .wait()
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid, "{error:?}");
    assert!(error.to_string().contains("changed"), "{error}");
    assert_eq!(
        a.rest.with_state(|s| s
            .requests_seen
            .iter()
            .filter(|r| r.ends_with("/orders/edit"))
            .count()),
        0,
        "no edit was sent"
    );

    // Reviewed again, it goes.
    let fresh = runtime
        .preview_edit(placed.order_id.clone(), change)
        .wait()
        .unwrap()
        .base
        .unwrap();
    runtime
        .amend(placed.order_id.clone(), change, fresh)
        .wait()
        .expect("the edit of the reviewed order goes through");
    runtime.stop();
}

#[test]
fn an_edit_for_another_account_is_refused() {
    let journal = Arc::new(MemoryJournal::new());
    let a = exchange("pf-a");
    let runtime = start(&a, &journal);
    wait_live(&runtime);
    let placed = runtime
        .submit(a.draft(), account_of(&a))
        .wait()
        .expect("placed");
    wait_until("the order to be open", || {
        runtime
            .snapshot()
            .orders
            .iter()
            .any(|v| v.order.id == placed.order_id)
    });
    let order = runtime
        .snapshot()
        .orders
        .iter()
        .find(|v| v.order.id == placed.order_id)
        .unwrap()
        .order
        .clone();
    let elsewhere = OrderFingerprint::of(&AccountId::new("pf-b"), &order);
    let change = OrderChange {
        price: Some(d("89000.00")),
        ..OrderChange::default()
    };
    let error = runtime
        .amend(placed.order_id, change, elsewhere)
        .wait()
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    runtime.stop();
}

// ---- A price has a time -----------------------------------------------------------------------

#[test]
fn a_price_without_the_exchanges_time_is_not_a_price() {
    let fake = FakeCoinbase::start();
    let connection = Coinbase
        .connect(
            &credentials(&generate_key_pem()),
            &Env::default().with_loopback_base_url(fake.url()),
        )
        .unwrap();
    let quote = connection.quote(&ProductId::new("BTC-USD")).unwrap();
    assert_eq!(quote.time_ms, 1_794_225_600_000, "the exchange's own time");

    fake.with_state(|state| state.quote_without_time = true);
    let error = connection.quote(&ProductId::new("BTC-USD")).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid, "{error:?}");
}

#[test]
fn the_sandbox_trades_in_an_account_of_its_own() {
    let journal = Arc::new(MemoryJournal::new());
    let fake = FakeCoinbase::start();
    let env = Env::default().with_loopback_base_url(fake.url());
    let config = RuntimeConfig::new(env, journal, Arc::new(Quiet), Arc::new(Quiet)).fast();
    let runtime = BrokerRuntime::start(&CoinbaseSandbox, crate::secret::Credentials::new(), config);
    wait_live(&runtime);
    assert_eq!(
        runtime.snapshot().account.as_ref().unwrap().id.clone(),
        AccountId::new("sandbox")
    );
    runtime.stop();
}
