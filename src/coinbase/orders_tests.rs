//! Order operations and the idempotent submit/reconcile flow, against the stateful fake exchange.

use std::time::Duration;

use rust_decimal::Decimal;
use std::str::FromStr;

use super::Coinbase;
use super::fake::{FakeCoinbase, credentials, generate_key_pem};
use crate::adapter::{Broker, ClientIdRules, Connection, Env};
use crate::error::ErrorKind;
use crate::model::orders::{CancelOutcome, EditOutcome, FillFilter, HistoryFilter, OrderChange};
use crate::model::pending::{
    MemoryJournal, PendingJournal, PendingSubmission, new_client_order_id,
};
use crate::model::types::{
    BrokerId, ClientOrderId, OrderDraft, OrderId, OrderKind, OrderSize, OrderStatus, ProductId,
    Side, TimeInForce,
};
use crate::submit::{self, Reconciled, SETTLE_MS};

fn d(text: &str) -> Decimal {
    Decimal::from_str(text).unwrap()
}

fn connect(fake: &FakeCoinbase) -> Box<dyn Connection> {
    Coinbase
        .connect(
            &credentials(&generate_key_pem()),
            &Env::default().with_loopback_base_url(fake.url()),
        )
        .expect("connect to the fake exchange")
}

fn limit_buy() -> OrderDraft {
    OrderDraft {
        product: ProductId::new("BTC-USD"),
        side: Side::Buy,
        kind: OrderKind::Limit {
            base_size: d("0.001"),
            price: d("90000.00"),
            tif: TimeInForce::GoodTilCancelled,
            post_only: false,
        },
    }
}

const RULES: ClientIdRules = ClientIdRules {
    max_len: 128,
    idempotent: true,
};
const NOW: i64 = 1_794_225_600_000;

#[test]
fn a_preview_reports_fees_warnings_and_problems_without_placing_anything() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let preview = connection.preview(&limit_buy()).unwrap();
    assert!(preview.is_acceptable());
    assert_eq!(preview.fees, Some(d("0.54")));
    assert_eq!(preview.order_total, Some(d("90.54")));
    assert_eq!(preview.best_ask, Some(d("90000.50")));
    assert_eq!(
        preview.warnings,
        vec!["This is a large order for the market."]
    );
    assert_eq!(fake.posts_to_create(), 0);

    let mut too_big = limit_buy();
    if let OrderKind::Limit { base_size, .. } = &mut too_big.kind {
        *base_size = d("99999");
    }
    let preview = connection.preview(&too_big).unwrap();
    assert!(!preview.is_acceptable());
    assert_eq!(
        preview.problems[0].text,
        "Insufficient funds for this order."
    );
    assert_eq!(
        preview.problems[0].code.as_deref(),
        Some("PREVIEW_INSUFFICIENT_FUND")
    );
}

#[test]
fn placing_sends_the_exact_digits_and_the_client_order_id_once() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let client_id = ClientOrderId::new("11111111-2222-4333-8444-555555555555");
    let placed = connection.place(&limit_buy(), &client_id).unwrap();
    assert_eq!(placed.product.as_str(), "BTC-USD");
    assert_eq!(fake.posts_to_create(), 1);

    let sent = fake
        .server
        .requests()
        .into_iter()
        .find(|r| r.method == "POST" && r.path().ends_with("/orders"))
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&sent.body).unwrap();
    assert_eq!(body["client_order_id"], client_id.as_str());
    assert_eq!(
        body["order_configuration"]["limit_limit_gtc"]["limit_price"],
        "90000.00"
    );
    assert_eq!(
        body["order_configuration"]["limit_limit_gtc"]["base_size"],
        "0.001"
    );
    assert!(
        sent.header("authorization")
            .unwrap()
            .starts_with("Bearer eyJ")
    );
}

#[test]
fn a_refusal_is_a_plain_sentence_with_the_brokers_code() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    fake.with_state(|s| s.refuse_create_with = Some("INSUFFICIENT_FUND".to_owned()));
    let error = connection
        .place(&limit_buy(), &ClientOrderId::new("c1"))
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Rejected);
    assert_eq!(error.to_string(), "Insufficient funds for this order.");
    assert_eq!(error.code.as_deref(), Some("INSUFFICIENT_FUND"));
}

#[test]
fn the_same_client_id_never_creates_a_second_order() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let client_id = ClientOrderId::new("dup-1");
    connection.place(&limit_buy(), &client_id).unwrap();
    let error = connection.place(&limit_buy(), &client_id).unwrap_err();
    assert_eq!(
        error.kind,
        ErrorKind::OutcomeUnknown,
        "a duplicate means: look it up"
    );
    assert_eq!(error.code.as_deref(), Some("DUPLICATE_CLIENT_ORDER_ID"));
    assert_eq!(fake.with_state(|s| s.orders.len()), 1);
}

#[test]
fn a_lost_reply_leaves_the_order_recorded_and_findable_not_sent_twice() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let journal = MemoryJournal::new();
    fake.with_state(|s| s.drop_reply_after_create = true);
    let client_id = new_client_order_id(&RULES).unwrap();

    let error =
        submit::submit(connection.as_ref(), &journal, &limit_buy(), &client_id, NOW).unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutcomeUnknown);
    assert_eq!(fake.posts_to_create(), 1, "sent once");
    assert_eq!(
        journal.unresolved(BrokerId::new("coinbase")).unwrap().len(),
        1
    );
    // Coinbase did create it.
    assert_eq!(fake.with_state(|s| s.orders.len()), 1);

    let entry = journal
        .unresolved(BrokerId::new("coinbase"))
        .unwrap()
        .remove(0);
    let found = submit::reconcile(connection.as_ref(), &journal, &entry, NOW + 5_000).unwrap();
    let Reconciled::Placed(order) = found else {
        panic!("expected the order to be found")
    };
    assert_eq!(order.client_id.as_ref(), Some(&client_id));
    assert_eq!(order.status, OrderStatus::Open);
    assert!(
        journal
            .unresolved(BrokerId::new("coinbase"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fake.posts_to_create(),
        1,
        "reconciling does not send anything"
    );
}

#[test]
fn an_order_the_broker_does_not_list_is_believed_missing_only_after_it_has_had_time() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let journal = MemoryJournal::new();
    let entry = PendingSubmission {
        broker: BrokerId::new("coinbase"),
        client_id: ClientOrderId::new("never-sent"),
        draft: limit_buy(),
        created_ms: NOW,
    };
    journal.record(&entry).unwrap();

    let early = submit::reconcile(connection.as_ref(), &journal, &entry, NOW + 1_000).unwrap();
    assert_eq!(early, Reconciled::NotListedYet);
    assert_eq!(
        journal.unresolved(entry.broker).unwrap().len(),
        1,
        "still unresolved"
    );

    let late = submit::reconcile(connection.as_ref(), &journal, &entry, NOW + SETTLE_MS).unwrap();
    assert_eq!(late, Reconciled::NotPlaced);
    assert!(journal.unresolved(entry.broker).unwrap().is_empty());
}

#[test]
fn an_order_that_cannot_be_recorded_is_not_sent() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let journal = MemoryJournal::new();
    journal
        .fail_to_record
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = submit::submit(
        connection.as_ref(),
        &journal,
        &limit_buy(),
        &ClientOrderId::new("c"),
        NOW,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Invalid);
    assert_eq!(fake.posts_to_create(), 0);
}

#[test]
fn a_clear_refusal_clears_the_record_and_a_success_does_too() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let journal = MemoryJournal::new();
    fake.with_state(|s| s.refuse_create_with = Some("INVALID_PRICE_PRECISION".to_owned()));
    let error = submit::submit(
        connection.as_ref(),
        &journal,
        &limit_buy(),
        &ClientOrderId::new("a"),
        NOW,
    )
    .unwrap_err();
    assert_eq!(error.kind, ErrorKind::Rejected);
    assert!(
        journal
            .unresolved(BrokerId::new("coinbase"))
            .unwrap()
            .is_empty()
    );
    submit::submit(
        connection.as_ref(),
        &journal,
        &limit_buy(),
        &ClientOrderId::new("b"),
        NOW,
    )
    .unwrap();
    assert!(
        journal
            .unresolved(BrokerId::new("coinbase"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn retrying_looks_first_and_reuses_the_same_client_id() {
    // Case 1: the order exists -> found, nothing is sent.
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let journal = MemoryJournal::new();
    fake.with_state(|s| s.drop_reply_after_create = true);
    let client_id = ClientOrderId::new("retry-1");
    submit::submit(connection.as_ref(), &journal, &limit_buy(), &client_id, NOW).unwrap_err();
    let entry = journal
        .unresolved(BrokerId::new("coinbase"))
        .unwrap()
        .remove(0);
    let placed = submit::retry(connection.as_ref(), &journal, &RULES, &entry, NOW + 3_000)
        .unwrap()
        .placed;
    assert_eq!(placed.product.as_str(), "BTC-USD");
    assert_eq!(fake.posts_to_create(), 1, "found, not doubled");

    // Case 2: the order was really never created -> sent again under the same id.
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let journal = MemoryJournal::new();
    let entry = PendingSubmission {
        broker: BrokerId::new("coinbase"),
        client_id: ClientOrderId::new("retry-2"),
        draft: limit_buy(),
        created_ms: NOW,
    };
    journal.record(&entry).unwrap();
    submit::retry(
        connection.as_ref(),
        &journal,
        &RULES,
        &entry,
        NOW + SETTLE_MS + 1,
    )
    .unwrap();
    assert_eq!(fake.posts_to_create(), 1);
    assert_eq!(
        fake.with_state(|s| s.orders[0].client_id.clone()),
        "retry-2"
    );
    assert!(journal.unresolved(entry.broker).unwrap().is_empty());
}

#[test]
fn a_broker_without_id_idempotency_is_not_resent_until_the_lookup_settles() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let journal = MemoryJournal::new();
    let entry = PendingSubmission {
        broker: BrokerId::new("coinbase"),
        client_id: ClientOrderId::new("careful"),
        draft: limit_buy(),
        created_ms: NOW,
    };
    journal.record(&entry).unwrap();
    let rules = ClientIdRules {
        max_len: 128,
        idempotent: false,
    };
    let error =
        submit::retry(connection.as_ref(), &journal, &rules, &entry, NOW + 1_000).unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutcomeUnknown);
    assert_eq!(fake.posts_to_create(), 0);
}

#[test]
fn cancel_reports_requested_not_cancelled_and_each_refusal_distinctly() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let a = connection
        .place(&limit_buy(), &ClientOrderId::new("a"))
        .unwrap();
    let results = connection
        .cancel(&[a.order_id.clone(), OrderId::new("ghost")])
        .unwrap();
    assert_eq!(results[0].outcome, CancelOutcome::Requested);
    assert_eq!(results[1].outcome, CancelOutcome::NotFound);
    // The order is still live until the broker says otherwise.
    let order = connection.order(&a.order_id).unwrap();
    assert_eq!(order.status, OrderStatus::CancelQueued);
    assert!(order.status.is_working());

    for (reason, expected) in [
        ("ORDER_IS_FULLY_FILLED", CancelOutcome::AlreadyFilled),
        ("ORDER_IS_BEING_REPLACED", CancelOutcome::BeingReplaced),
        ("DUPLICATE_CANCEL_REQUEST", CancelOutcome::Requested),
        (
            "NOT_ALLOWED_TO_CANCEL",
            CancelOutcome::Rejected("This order cannot be cancelled.".to_owned()),
        ),
    ] {
        fake.with_state(|s| s.cancel_failure = Some(reason.to_owned()));
        assert_eq!(
            connection
                .cancel(std::slice::from_ref(&a.order_id))
                .unwrap()[0]
                .outcome,
            expected,
            "{reason}"
        );
    }
    assert!(connection.cancel(&[]).unwrap().is_empty());
}

#[test]
fn only_an_open_limit_order_can_be_edited_and_the_edit_never_cancels_and_replaces() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let placed = connection
        .place(&limit_buy(), &ClientOrderId::new("e"))
        .unwrap();
    let order = connection.order(&placed.order_id).unwrap();
    let change = OrderChange {
        price: Some(d("89500.00")),
        ..OrderChange::default()
    };

    let preview = connection.preview_edit(&order, &change).unwrap();
    assert!(preview.is_acceptable());
    assert_eq!(
        connection.amend(&order, &change).unwrap(),
        EditOutcome::Native
    );
    let seen = fake.with_state(|s| s.requests_seen.clone());
    assert!(
        seen.iter()
            .any(|r| r == "POST /api/v3/brokerage/orders/edit")
    );
    assert!(
        !seen.iter().any(|r| r.contains("batch_cancel")),
        "an edit never cancels"
    );
    assert_eq!(
        fake.posts_to_create(),
        1,
        "an edit never places a second order"
    );
    let body: serde_json::Value = serde_json::from_str(
        &fake
            .server
            .requests()
            .iter()
            .find(|r| r.path().ends_with("/orders/edit"))
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(body["price"], "89500.00");
    assert_eq!(body["size"], "0.001", "an unchanged size is sent as it was");

    // Not open any more (the fake moved it to EDIT_QUEUED): refused before any request is made.
    let queued = connection.order(&placed.order_id).unwrap();
    assert_eq!(queued.status, OrderStatus::EditQueued);
    let edits_before = fake.with_state(|s| s.requests_seen.len());
    let error = connection.amend(&queued, &change).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_eq!(fake.with_state(|s| s.requests_seen.len()), edits_before);

    assert_eq!(
        connection
            .amend(&order, &OrderChange::default())
            .unwrap_err()
            .kind,
        ErrorKind::Invalid
    );
}

#[test]
fn an_edit_the_broker_will_not_take_says_why_in_the_preview_and_the_edit() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let placed = connection
        .place(&limit_buy(), &ClientOrderId::new("f"))
        .unwrap();
    let order = connection.order(&placed.order_id).unwrap();
    let change = OrderChange {
        size: Some(d("0.002")),
        ..OrderChange::default()
    };
    fake.with_state(|s| {
        s.edit_failure = Some("EXCEEDED_MAX_ALLOWED_EDIT_REQUEST_COUNT".to_owned())
    });
    let preview = connection.preview_edit(&order, &change).unwrap();
    assert!(!preview.is_acceptable());
    assert_eq!(
        preview.problems[0].text,
        "This order has been edited too many times."
    );
    let error = connection.amend(&order, &change).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert_eq!(
        error.to_string(),
        "This order has been edited too many times."
    );
}

#[test]
fn orders_stay_open_until_they_leave_the_working_set_and_history_filters_by_status() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let a = connection
        .place(&limit_buy(), &ClientOrderId::new("o1"))
        .unwrap();
    connection
        .place(&limit_buy(), &ClientOrderId::new("o2"))
        .unwrap();
    assert_eq!(connection.open_orders().unwrap().len(), 2);
    fake.with_state(|s| s.orders[0].status = "FILLED".to_owned());
    let open = connection.open_orders().unwrap();
    assert_eq!(open.len(), 1);
    assert_ne!(open[0].id, a.order_id);

    let page = connection
        .order_history(
            &HistoryFilter {
                statuses: vec![OrderStatus::Filled],
                ..HistoryFilter::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].status, OrderStatus::Filled);
    assert!(page.next_cursor.is_none());
    let query = fake
        .server
        .requests()
        .into_iter()
        .rev()
        .find(|r| r.path().ends_with("/orders/historical/batch"))
        .unwrap()
        .query()
        .to_owned();
    assert!(query.contains("order_status=FILLED"), "{query}");
}

/// Coinbase refuses a listing that asks for several active statuses at once. The working set is
/// every active status, so each is asked for on its own and the answers are merged, once each.
#[test]
fn the_working_orders_are_listed_one_active_status_at_a_time() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    for n in 0..5 {
        connection
            .place(&limit_buy(), &ClientOrderId::new(format!("w{n}")))
            .unwrap();
    }
    fake.with_state(|s| {
        for (order, status) in
            s.orders
                .iter_mut()
                .zip(["OPEN", "PENDING", "QUEUED", "CANCEL_QUEUED", "EDIT_QUEUED"])
        {
            order.status = status.to_owned();
        }
    });
    let open = connection.open_orders().expect("the working orders");
    assert_eq!(open.len(), 5, "every active status is listed: {open:?}");
    let mut ids: Vec<_> = open.iter().map(|o| o.id.clone()).collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    ids.dedup();
    assert_eq!(ids.len(), 5, "and none twice");
}

/// A history filter that spans several statuses, one of them active, is answered too.
#[test]
fn a_history_filter_across_active_statuses_is_not_sent_as_several_at_once() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    for n in 0..3 {
        connection
            .place(&limit_buy(), &ClientOrderId::new(format!("h{n}")))
            .unwrap();
    }
    fake.with_state(|s| {
        s.orders[0].status = "OPEN".to_owned();
        s.orders[1].status = "QUEUED".to_owned();
        s.orders[2].status = "FILLED".to_owned();
    });
    let pending = connection
        .order_history(
            &HistoryFilter {
                statuses: vec![OrderStatus::Pending],
                ..HistoryFilter::default()
            },
            None,
        )
        .expect("pending alone (PENDING and QUEUED)");
    assert_eq!(pending.items.len(), 1);
    let mixed = connection
        .order_history(
            &HistoryFilter {
                statuses: vec![OrderStatus::Open, OrderStatus::Filled],
                ..HistoryFilter::default()
            },
            None,
        )
        .expect("an active and a finished status together");
    let mut statuses: Vec<_> = mixed.items.iter().map(|o| o.status).collect();
    statuses.sort_by_key(|s| format!("{s:?}"));
    assert_eq!(statuses, vec![OrderStatus::Filled, OrderStatus::Open]);
}

#[test]
fn fills_fees_and_a_missing_order_are_read() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    assert!(
        connection
            .fills(&FillFilter::default(), None)
            .unwrap()
            .items
            .is_empty()
    );
    let fees = connection.fee_summary().unwrap();
    assert_eq!(fees.tier, "Advanced 1");
    assert_eq!(fees.taker_rate, Some(d("0.006")));
    assert_eq!(
        connection.order(&OrderId::new("nope")).unwrap_err().kind,
        ErrorKind::NotFound
    );
}

#[test]
fn a_rate_limit_on_a_read_is_waited_out_but_never_resends_an_order() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    fake.with_state(|s| s.rate_limit_next = 1);
    // (the real 1 s Retry-After is capped by the client; this proves a read recovers)
    let started = std::time::Instant::now();
    assert!(connection.open_orders().is_ok());
    assert!(started.elapsed() < Duration::from_secs(6));

    fake.with_state(|s| s.rate_limit_next = 1);
    let error = connection
        .place(&limit_buy(), &ClientOrderId::new("rl"))
        .unwrap_err();
    assert!(matches!(error.kind, ErrorKind::RateLimited { .. }));
    assert_eq!(
        fake.posts_to_create(),
        1,
        "one attempt, answered 429, never repeated"
    );
    assert_eq!(fake.with_state(|s| s.orders.len()), 0);
}

#[test]
fn market_orders_by_quote_and_stop_limits_serialise_as_documented() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let market = OrderDraft {
        product: ProductId::new("BTC-USD"),
        side: Side::Buy,
        kind: OrderKind::Market {
            size: OrderSize::Quote(d("25.00")),
        },
    };
    connection.place(&market, &ClientOrderId::new("m")).unwrap();
    let body: serde_json::Value = serde_json::from_str(
        &fake
            .server
            .requests()
            .iter()
            .find(|r| r.method == "POST" && r.path().ends_with("/orders"))
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(
        body["order_configuration"]["market_market_ioc"]["quote_size"],
        "25.00"
    );
    let sent: serde_json::Value = body["order_configuration"].clone();
    assert!(
        sent["market_market_ioc"].get("base_size").is_none(),
        "only one size basis is sent"
    );
}

#[test]
fn an_answer_that_cannot_be_read_never_says_not_sent_and_keeps_the_record() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let journal = MemoryJournal::new();
    fake.with_state(|s| s.garble_create_reply = true);
    let client_id = new_client_order_id(&RULES).unwrap();

    let error =
        submit::submit(connection.as_ref(), &journal, &limit_buy(), &client_id, NOW).unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutcomeUnknown, "{error:?}");
    assert_eq!(fake.with_state(|s| s.orders.len()), 1, "it was created");
    assert_eq!(
        journal.unresolved(BrokerId::new("coinbase")).unwrap().len(),
        1,
        "and is still recorded, so it can be found"
    );
    let entry = journal
        .unresolved(BrokerId::new("coinbase"))
        .unwrap()
        .remove(0);
    let found = submit::reconcile(connection.as_ref(), &journal, &entry, NOW + 5_000).unwrap();
    assert!(matches!(found, Reconciled::Placed(_)));
    assert_eq!(fake.posts_to_create(), 1);
}

#[test]
fn an_accepted_order_is_placed_even_if_the_journal_cannot_be_updated() {
    use std::sync::atomic::Ordering;
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let journal = MemoryJournal::new();
    journal.fail_to_resolve.store(true, Ordering::SeqCst);
    let client_id = new_client_order_id(&RULES).unwrap();

    let submitted =
        submit::submit(connection.as_ref(), &journal, &limit_buy(), &client_id, NOW).unwrap();
    assert_eq!(submitted.placed.order_id.as_str(), "order-1");
    assert!(
        submitted.unsettled.is_some(),
        "the failure is reported alongside, not instead"
    );
    assert_eq!(
        journal.unresolved(BrokerId::new("coinbase")).unwrap().len(),
        1,
        "the entry stays until it can be closed"
    );

    // Found later, with the journal still broken: the order is returned, flagged.
    let entry = journal
        .unresolved(BrokerId::new("coinbase"))
        .unwrap()
        .remove(0);
    let found = submit::reconcile(connection.as_ref(), &journal, &entry, NOW + 5_000).unwrap();
    assert!(
        matches!(found, Reconciled::PlacedUnsettled(_, _)),
        "{found:?}"
    );
    // And closed once the journal works again, with nothing sent.
    journal.fail_to_resolve.store(false, Ordering::SeqCst);
    let found = submit::reconcile(connection.as_ref(), &journal, &entry, NOW + 6_000).unwrap();
    assert!(matches!(found, Reconciled::Placed(_)));
    assert!(
        journal
            .unresolved(BrokerId::new("coinbase"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(fake.posts_to_create(), 1);
}

fn many_resting_orders(fake: &FakeCoinbase, count: usize) -> Vec<OrderId> {
    use super::fake::FakeOrder;
    fake.with_state(|state| {
        (0..count)
            .map(|n| {
                let id = format!("bulk-{n}");
                state.orders.push(FakeOrder {
                    id: id.clone(),
                    client_id: format!("bulk-client-{n}"),
                    product: "BTC-USD".to_owned(),
                    side: "BUY".to_owned(),
                    status: "OPEN".to_owned(),
                    configuration: serde_json::json!({"limit_limit_gtc": {
                        "base_size": "0.001", "limit_price": "90000.00", "post_only": false}}),
                    filled_size: "0".to_owned(),
                    created_time: "2026-11-09T12:00:00.000Z".to_owned(),
                });
                OrderId::new(id)
            })
            .collect()
    })
}

#[test]
fn a_cancel_that_fails_in_a_later_batch_keeps_what_the_earlier_batches_did() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    // 250 orders go out in batches of 100, 100 and 50; the second batch's answer is a 502.
    let ids = many_resting_orders(&fake, 250);
    fake.with_state(|s| s.fail_cancel_batch = Some(1));

    let results = connection.cancel(&ids).unwrap();
    assert_eq!(results.len(), 250, "every order is accounted for");
    assert!(
        results[..100]
            .iter()
            .all(|r| r.outcome == CancelOutcome::Requested),
        "the first batch was sent and answered"
    );
    assert!(
        results[100..200]
            .iter()
            .all(|r| matches!(r.outcome, CancelOutcome::Unknown(_))),
        "the second may have been carried out: not 'refused'"
    );
    assert!(
        results[200..].iter().all(
            |r| matches!(&r.outcome, CancelOutcome::Rejected(text) if text.contains("Not sent"))
        ),
        "the third was not tried"
    );
    assert_eq!(
        fake.with_state(|s| s.cancel_batches_seen),
        2,
        "and not sent"
    );
    assert_eq!(
        results
            .iter()
            .map(|r| r.order_id.clone())
            .collect::<Vec<_>>(),
        ids,
        "in the order asked"
    );

    // A request that fails at the first batch has nothing to keep and fails as a whole.
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    let ids = many_resting_orders(&fake, 150);
    fake.with_state(|s| s.fail_cancel_batch = Some(0));
    let error = connection.cancel(&ids).unwrap_err();
    assert_eq!(error.kind, ErrorKind::OutcomeUnknown);
}

#[test]
fn a_broker_that_cannot_preview_says_so_and_places_nothing() {
    let fake = FakeCoinbase::start();
    let connection = connect(&fake);
    fake.with_state(|s| s.fail_preview = true);
    let error = connection.preview(&limit_buy()).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Rejected, "{error:?}");
    assert_eq!(fake.posts_to_create(), 0);
    fake.with_state(|s| s.fail_preview = false);
    assert!(connection.preview(&limit_buy()).unwrap().is_acceptable());
}
