//! Scenarios every broker adapter must pass, driven through [`BrokerRuntime`] against that
//! broker's mock exchange. A new adapter implements [`Exchange`] over its mock and runs
//! [`run_all`]; nothing here names a broker.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::adapter::{Broker, Clock};
use crate::error::ErrorKind;
use crate::model::pending::{MemoryJournal, PendingJournal};
use crate::model::reducer::{Notice, NoticeKind};
use crate::model::types::{OrderDraft, OrderId, OrderStatus};
use crate::runtime::{
    BrokerRuntime, BrokerSnapshot, NoticeSink, PendingState, RuntimeConfig, Status, Waker,
};
use crate::secret::Credentials;
use crate::testing::ManualClock;

/// Control of a broker's mock exchange: what a test needs to make the exchange misbehave.
pub trait Exchange: Send + Sync {
    fn broker(&self) -> &'static dyn Broker;
    fn credentials(&self) -> Credentials;
    fn env(&self, clock: Arc<dyn Clock>) -> crate::adapter::Env;
    /// A valid resting buy order in a market the mock lists.
    fn draft(&self) -> OrderDraft;
    /// The same order with a price off the market's increment.
    fn off_increment_draft(&self) -> OrderDraft;

    /// The exchange creates the next order and then the reply is lost on the wire.
    fn lose_next_order_reply(&self);
    fn refuse_next_order(&self);
    /// Orders the exchange holds.
    fn orders_created(&self) -> usize;
    /// Order-creating requests the exchange received.
    fn order_posts(&self) -> usize;
    fn order_ids(&self) -> Vec<String>;
    /// Changes an order at the exchange; `announce` also sends it down the live stream.
    fn change_order(&self, id: &str, status: &str, filled: &str, announce: bool);
    /// Sends the same state down the stream again (a duplicate).
    fn announce_again(&self, id: &str, status: &str, filled: &str);
    fn cancel_will_fail_because_filled(&self);
    fn drop_stream(&self);
    fn stream_connections(&self) -> u64;
    fn revoke_key(&self);
    fn rate_limit_next(&self, requests: usize);
}

#[derive(Default)]
struct CountingWaker(AtomicUsize);

impl Waker for CountingWaker {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Notices(Mutex<Vec<Notice>>);

impl NoticeSink for Notices {
    fn notice(&self, notice: Notice) {
        self.0.lock().expect("notices").push(notice);
    }
}

impl Notices {
    fn kinds(&self) -> Vec<NoticeKind> {
        self.0
            .lock()
            .expect("notices")
            .iter()
            .map(|n| n.kind)
            .collect()
    }
}

struct Fixture<'a> {
    exchange: &'a dyn Exchange,
    runtime: BrokerRuntime,
    journal: Arc<MemoryJournal>,
    notices: Arc<Notices>,
    waker: Arc<CountingWaker>,
    clock: Arc<ManualClock>,
}

const WAIT: Duration = Duration::from_secs(8);

fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

impl<'a> Fixture<'a> {
    fn start(exchange: &'a dyn Exchange) -> Self {
        Self::start_with(exchange, Arc::new(MemoryJournal::new()))
    }

    fn start_with(exchange: &'a dyn Exchange, journal: Arc<MemoryJournal>) -> Self {
        let clock = Arc::new(ManualClock::new(1_794_225_600_000));
        let notices = Arc::new(Notices::default());
        let waker = Arc::new(CountingWaker::default());
        let config = RuntimeConfig::new(
            exchange.env(clock.clone()),
            journal.clone(),
            waker.clone(),
            notices.clone(),
        )
        .fast();
        let runtime = BrokerRuntime::start(exchange.broker(), exchange.credentials(), config);
        Self {
            exchange,
            runtime,
            journal,
            notices,
            waker,
            clock,
        }
    }

    fn snapshot(&self) -> Arc<BrokerSnapshot> {
        self.runtime.snapshot()
    }

    fn wait_live(&self) {
        wait_until("the runtime to go live", || self.snapshot().is_live());
    }

    fn order(&self, id: &str) -> Option<crate::model::reducer::OrderView> {
        self.snapshot()
            .orders
            .iter()
            .find(|v| v.order.id.as_str() == id)
            .cloned()
    }

    fn wait_order(
        &self,
        id: &str,
        what: &str,
        mut ready: impl FnMut(&crate::model::reducer::OrderView) -> bool,
    ) {
        wait_until(what, || self.order(id).is_some_and(|v| ready(&v)));
    }

    fn place(&self) -> String {
        let placed = self
            .runtime
            .submit(self.exchange.draft())
            .wait()
            .expect("the order is placed");
        let id = placed.order_id.0;
        self.wait_order(&id, "the new order to appear", |v| {
            v.order.status == OrderStatus::Open
        });
        id
    }

    fn broker_id(&self) -> crate::model::types::BrokerId {
        self.exchange.broker().descriptor().id
    }
}

/// Every scenario, each against a fresh exchange from `fresh`.
pub fn run_all(fresh: impl Fn() -> Box<dyn Exchange>) {
    type Scenario = fn(&dyn Exchange);
    let scenarios: &[(&str, Scenario)] = &[
        (
            "connects and reads the account",
            connects_and_reads_the_account,
        ),
        ("place, partial fill, fill", place_partial_fill_fill),
        (
            "duplicate and stale stream events",
            duplicate_and_stale_stream_events,
        ),
        (
            "cancel is requested, not done",
            cancel_is_requested_not_done,
        ),
        ("cancel races with a fill", cancel_races_with_a_fill),
        ("a lost reply is reconciled", a_lost_reply_is_reconciled),
        (
            "a refused order leaves nothing behind",
            a_refused_order_leaves_nothing_behind,
        ),
        ("an invalid order is not sent", an_invalid_order_is_not_sent),
        ("a stream drop resyncs", a_stream_drop_resyncs),
        ("waking from sleep resyncs", waking_from_sleep_resyncs),
        ("a revoked key stops trading", a_revoked_key_stops_trading),
        (
            "a rate limit does not resend an order",
            a_rate_limit_does_not_resend_an_order,
        ),
        (
            "an unreadable journal blocks new orders but not cancels",
            an_unreadable_journal_blocks_new_orders_but_not_cancels,
        ),
        (
            "an unreadable journal at start is reported, not ignored",
            an_unreadable_journal_at_start_is_reported_not_ignored,
        ),
        (
            "an accepted order stays visible when its record cannot be closed",
            an_accepted_order_stays_visible_when_its_record_cannot_be_closed,
        ),
        (
            "a failed lookup keeps the unconfirmed order",
            a_failed_lookup_keeps_the_unconfirmed_order,
        ),
    ];
    for (name, scenario) in scenarios {
        let exchange = fresh();
        eprintln!("conformance: {name}");
        scenario(exchange.as_ref());
    }
}

fn connects_and_reads_the_account(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    let snapshot = f.snapshot();
    assert!(snapshot.session.is_some());
    assert!(!snapshot.balances.is_empty(), "balances are read");
    assert!(!snapshot.products.is_empty(), "the market list is read");
    assert!(
        snapshot
            .products
            .iter()
            .all(|p| snapshot.capabilities_of(&p.id).is_some()),
        "what can be done in each market is known"
    );
    assert!(snapshot.last_sync_ms.is_some());
    assert!(
        f.waker.0.load(Ordering::SeqCst) > 0,
        "the interface is told to redraw"
    );
    assert!(f.runtime.revision() >= snapshot.revision);
    f.runtime.stop();
}

fn place_partial_fill_fill(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    let id = f.place();

    exchange.change_order(&id, "OPEN", "0.0004", true);
    f.wait_order(&id, "the partial fill", |v| {
        v.order.filled_size.to_string() == "0.0004"
    });
    exchange.change_order(&id, "FILLED", "0.001", true);
    f.wait_order(&id, "the fill", |v| v.order.status == OrderStatus::Filled);
    wait_until("the fills to be read", || f.snapshot().fills.len() == 2);

    assert_eq!(
        f.notices.kinds(),
        [
            NoticeKind::Accepted,
            NoticeKind::PartiallyFilled,
            NoticeKind::Filled
        ],
        "one notice per event"
    );
    assert!(f.snapshot().working_orders().next().is_none());
    f.runtime.stop();
}

fn duplicate_and_stale_stream_events(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    let id = f.place();
    exchange.change_order(&id, "OPEN", "0.0004", true);
    f.wait_order(&id, "the partial fill", |v| {
        v.order.filled_size.to_string() == "0.0004"
    });
    // The same event three more times, then an older state arriving late.
    for _ in 0..3 {
        exchange.announce_again(&id, "OPEN", "0.0004");
    }
    exchange.announce_again(&id, "OPEN", "0.0001");
    exchange.change_order(&id, "FILLED", "0.001", true);
    exchange.announce_again(&id, "OPEN", "0.0004"); // "still open" after it filled
    f.wait_order(&id, "the fill", |v| v.order.status == OrderStatus::Filled);
    std::thread::sleep(Duration::from_millis(150));
    let view = f.order(&id).expect("the order");
    assert_eq!(
        view.order.status,
        OrderStatus::Filled,
        "a finished order stays finished"
    );
    assert_eq!(
        view.order.filled_size.to_string(),
        "0.001",
        "the filled amount never goes down"
    );
    assert_eq!(
        f.notices.kinds(),
        [
            NoticeKind::Accepted,
            NoticeKind::PartiallyFilled,
            NoticeKind::Filled
        ]
    );
    f.runtime.stop();
}

fn cancel_is_requested_not_done(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    let id = f.place();
    let results = f
        .runtime
        .cancel(vec![OrderId::new(&id)])
        .wait()
        .expect("the cancel is sent");
    assert_eq!(results.len(), 1);

    let view = f.order(&id).expect("the order is still listed");
    assert!(view.order.status.is_working(), "asking is not cancelling");
    // The note is there until the exchange answers.
    wait_until("the exchange's answer", || {
        f.order(&id)
            .is_some_and(|v| v.order.status == OrderStatus::CancelQueued)
    });
    exchange.change_order(&id, "CANCELLED", "0", true);
    f.wait_order(&id, "the cancellation", |v| {
        v.order.status == OrderStatus::Cancelled
    });
    assert!(f.order(&id).unwrap().overlay.is_none());
    assert!(f.notices.kinds().contains(&NoticeKind::Cancelled));
    f.runtime.stop();
}

fn cancel_races_with_a_fill(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    let id = f.place();
    // The order fills at the exchange; nothing has told us yet; the user clicks cancel.
    exchange.change_order(&id, "FILLED", "0.001", false);
    exchange.cancel_will_fail_because_filled();
    let results = f
        .runtime
        .cancel(vec![OrderId::new(&id)])
        .wait()
        .expect("the cancel is sent");
    assert!(matches!(
        results[0].outcome,
        crate::model::orders::CancelOutcome::AlreadyFilled
    ));
    f.wait_order(&id, "the order to be read as filled", |v| {
        v.order.status == OrderStatus::Filled
    });
    assert!(f.order(&id).unwrap().overlay.is_none());
    assert!(f.notices.kinds().contains(&NoticeKind::Filled));
    f.runtime.stop();
}

fn a_lost_reply_is_reconciled(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    exchange.lose_next_order_reply();
    let error = f.runtime.submit(exchange.draft()).wait().unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutcomeUnknown, "{error:?}");
    assert_eq!(exchange.order_posts(), 1, "the order was sent once");
    assert_eq!(exchange.orders_created(), 1);
    // Until it is found it is shown as unconfirmed, and nothing else may be assumed.
    assert!(
        f.snapshot()
            .pending
            .iter()
            .all(|p| p.state == PendingState::Unknown)
    );
    wait_until("the lookup to find the order", || {
        f.snapshot().pending.is_empty()
    });
    assert_eq!(
        f.snapshot().working_orders().count(),
        1,
        "the order is now shown"
    );
    assert!(f.journal.unresolved(f.broker_id()).unwrap().is_empty());
    assert_eq!(exchange.order_posts(), 1, "reconciling sends nothing");
    f.runtime.stop();
}

fn a_refused_order_leaves_nothing_behind(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    exchange.refuse_next_order();
    let error = f.runtime.submit(exchange.draft()).wait().unwrap_err();
    assert_eq!(error.kind, ErrorKind::Rejected);
    assert!(f.snapshot().pending.is_empty());
    assert!(f.journal.unresolved(f.broker_id()).unwrap().is_empty());
    assert_eq!(exchange.orders_created(), 0);
    f.runtime.stop();
}

fn an_invalid_order_is_not_sent(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    let error = f
        .runtime
        .submit(exchange.off_increment_draft())
        .wait()
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert!(error.to_string().contains("multiple"), "{error}");
    assert_eq!(
        exchange.order_posts(),
        0,
        "an invalid order never reaches the exchange"
    );
    f.runtime.stop();
}

fn a_stream_drop_resyncs(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    let id = f.place();
    let connections = exchange.stream_connections();
    exchange.drop_stream();
    // While the stream is down the order fills; no message says so.
    exchange.change_order(&id, "FILLED", "0.001", false);
    wait_until("a second stream connection", || {
        exchange.stream_connections() > connections
    });
    f.wait_order(&id, "the missed fill to be read", |v| {
        v.order.status == OrderStatus::Filled
    });
    wait_until("live again", || f.snapshot().is_live());
    assert_eq!(
        f.notices
            .kinds()
            .iter()
            .filter(|k| **k == NoticeKind::Filled)
            .count(),
        1,
        "told once"
    );
    f.runtime.stop();
}

fn waking_from_sleep_resyncs(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    let id = f.place();
    exchange.change_order(&id, "FILLED", "0.001", false);
    // A long gap between two ticks: the machine slept.
    f.clock.advance(60_000);
    f.wait_order(&id, "the fill to be found after waking", |v| {
        v.order.status == OrderStatus::Filled
    });
    f.runtime.stop();
}

fn a_revoked_key_stops_trading(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    exchange.revoke_key();
    f.runtime.refresh();
    wait_until("the failure", || {
        matches!(f.snapshot().status, Status::Failed(_))
    });
    let Status::Failed(error) = &f.snapshot().status else {
        unreachable!()
    };
    assert_eq!(error.kind, ErrorKind::Auth);
    let refused = f.runtime.submit(exchange.draft()).wait().unwrap_err();
    assert_ne!(refused.kind, ErrorKind::OutcomeUnknown);
    assert_eq!(exchange.order_posts(), 0);
    f.runtime.stop();
}

fn a_rate_limit_does_not_resend_an_order(exchange: &dyn Exchange) {
    let f = Fixture::start(exchange);
    f.wait_live();
    exchange.rate_limit_next(1);
    match f.runtime.submit(exchange.draft()).wait() {
        Err(error) => {
            assert!(
                matches!(
                    error.kind,
                    ErrorKind::RateLimited { .. } | ErrorKind::OutcomeUnknown
                ),
                "{error:?}"
            );
            assert_eq!(exchange.order_posts(), 1, "one attempt, never repeated");
        }
        // A background read took the one rate-limited answer; the order went through once.
        Ok(_) => assert_eq!(exchange.order_posts(), 1),
    }
    f.runtime.stop();
}

fn an_unreadable_journal_blocks_new_orders_but_not_cancels(exchange: &dyn Exchange) {
    use std::sync::atomic::Ordering;
    let f = Fixture::start(exchange);
    f.wait_live();
    let id = f.place();
    let posts = exchange.order_posts();

    // The record of unconfirmed orders becomes unreadable: orders that may be live could be missing
    // from it, so nothing new is sent, and the user is told why.
    f.journal.fail_to_read.store(true, Ordering::SeqCst);
    wait_until("the unreadable record to be noticed", || {
        f.snapshot().journal_error.is_some()
    });
    assert!(!f.snapshot().can_send_orders());
    let error = f.runtime.submit(exchange.draft()).wait().unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert!(error.to_string().contains("No order was sent"), "{error}");
    assert_eq!(
        exchange.order_posts(),
        posts,
        "nothing reached the exchange"
    );

    // Cancelling is still possible: the user must be able to get out.
    let results = f
        .runtime
        .cancel(vec![OrderId::new(&id)])
        .wait()
        .expect("the cancel is sent");
    assert!(matches!(
        results[0].outcome,
        crate::model::orders::CancelOutcome::Requested
    ));

    // Readable again: the block lifts by itself.
    f.journal.fail_to_read.store(false, Ordering::SeqCst);
    wait_until("the record to be read again", || {
        f.snapshot().journal_error.is_none()
    });
    assert!(f.snapshot().can_send_orders());
    f.runtime.stop();
}

fn an_unreadable_journal_at_start_is_reported_not_ignored(exchange: &dyn Exchange) {
    use std::sync::atomic::Ordering;
    let journal = Arc::new(MemoryJournal::new());
    journal.fail_to_read.store(true, Ordering::SeqCst);
    let f = Fixture::start_with(exchange, journal);
    f.wait_live();
    assert!(
        f.snapshot().journal_error.is_some(),
        "an unreadable record is not an empty one"
    );
    assert!(f.runtime.submit(exchange.draft()).wait().is_err());
    assert_eq!(exchange.order_posts(), 0);
    f.runtime.stop();
}

fn an_accepted_order_stays_visible_when_its_record_cannot_be_closed(exchange: &dyn Exchange) {
    use std::sync::atomic::Ordering;
    let f = Fixture::start(exchange);
    f.wait_live();
    f.journal.fail_to_resolve.store(true, Ordering::SeqCst);
    let placed = f
        .runtime
        .submit(exchange.draft())
        .wait()
        .expect("an accepted order is a placed order, whatever the journal says");
    f.wait_order(&placed.order_id.0, "the order to be shown", |_| true);
    assert_eq!(
        f.journal.unresolved(f.broker_id()).unwrap().len(),
        1,
        "the record stays until it can be closed"
    );
    assert!(
        f.snapshot()
            .last_error
            .as_deref()
            .is_some_and(|text| text.contains("record")),
        "the user is told: {:?}",
        f.snapshot().last_error
    );

    // Closed by itself once the journal works, with nothing sent again.
    f.journal.fail_to_resolve.store(false, Ordering::SeqCst);
    wait_until("the record to be closed", || {
        f.journal.unresolved(f.broker_id()).unwrap().is_empty()
    });
    assert_eq!(exchange.order_posts(), 1);
    assert_eq!(f.snapshot().working_orders().count(), 1);
    f.runtime.stop();
}

fn a_failed_lookup_keeps_the_unconfirmed_order(exchange: &dyn Exchange) {
    use crate::model::pending::{PendingJournal, PendingSubmission};
    let journal = Arc::new(MemoryJournal::new());
    let client_id = crate::model::types::ClientOrderId::new("sent-before-the-restart");
    let broker = exchange.broker().descriptor().id;
    journal
        .record(&PendingSubmission {
            broker,
            client_id: client_id.clone(),
            draft: exchange.draft(),
            created_ms: 1_794_225_600_000,
        })
        .unwrap();
    let f = Fixture::start_with(exchange, journal);
    f.wait_live();
    assert_eq!(f.snapshot().pending.len(), 1, "shown as unconfirmed");

    // The lookup fails (the key stops working): the order may still be live, so it stays listed
    // and recorded. It is not "gone" just because one lookup did not answer.
    exchange.revoke_key();
    let error = f.runtime.retry(client_id.clone()).wait().unwrap_err();
    assert_ne!(error.kind, ErrorKind::NotFound, "{error:?}");
    assert_eq!(f.snapshot().pending.len(), 1);
    assert_eq!(f.journal.unresolved(broker).unwrap().len(), 1);
    assert_eq!(exchange.order_posts(), 0, "a failed lookup sends nothing");
    f.runtime.stop();
}
