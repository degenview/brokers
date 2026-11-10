//! The user stream against the fake WebSocket server.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::Coinbase;
use super::fake::{
    FakeCoinbase, FakeStream, credentials, generate_key_pem, user_message, user_order,
};
use crate::adapter::{Broker, Connection, Env, StreamTiming};
use crate::model::events::{EventSink, StreamEvent};
use crate::model::types::OrderStatus;

#[derive(Default)]
struct Collector(Mutex<Vec<StreamEvent>>);

impl EventSink for Collector {
    fn event(&self, event: StreamEvent) {
        self.0.lock().unwrap().push(event);
    }
}

impl Collector {
    fn events(&self) -> Vec<StreamEvent> {
        self.0.lock().unwrap().clone()
    }

    fn wait_for(&self, what: &str, mut ready: impl FnMut(&[StreamEvent]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if ready(&self.events()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}; saw {:?}", self.events());
    }
}

fn connection(rest: &FakeCoinbase, stream: &FakeStream) -> Box<dyn Connection> {
    Coinbase
        .connect(
            &credentials(&generate_key_pem()),
            &Env::default()
                .with_loopback_base_url(rest.url())
                .with_loopback_ws_url(stream.url.clone())
                .with_stream_timing(StreamTiming::fast()),
        )
        .unwrap()
}

fn connected_count(events: &[StreamEvent]) -> usize {
    events
        .iter()
        .filter(|e| **e == StreamEvent::Connected)
        .count()
}

#[test]
fn subscribing_sends_a_fresh_token_per_channel_and_delivers_snapshot_then_updates() {
    let rest = FakeCoinbase::start();
    let stream = FakeStream::start();
    stream.set_snapshot(&user_message(
        "snapshot",
        &[user_order("o1", "c1", "OPEN", "0", "0.001")],
    ));
    let sink = Arc::new(Collector::default());
    let mut handle = connection(&rest, &stream)
        .open_stream(sink.clone())
        .unwrap();

    sink.wait_for("the snapshot", |e| {
        e.iter()
            .any(|e| matches!(e, StreamEvent::Orders { snapshot: true, .. }))
    });
    stream.push(&user_message(
        "update",
        &[user_order("o1", "c1", "OPEN", "0.0004", "0.0006")],
    ));
    sink.wait_for("the update", |e| {
        e.iter().any(|e| {
            matches!(
                e,
                StreamEvent::Orders {
                    snapshot: false,
                    ..
                }
            )
        })
    });

    let events = sink.events();
    assert_eq!(events[0], StreamEvent::Connected);
    let StreamEvent::Orders {
        orders,
        snapshot: false,
    } = events.last().unwrap()
    else {
        panic!("the last event should be the update");
    };
    assert_eq!(orders[0].status, OrderStatus::Open);
    assert_eq!(orders[0].filled_size.to_string(), "0.0004");

    let subscribes = stream.subscribes();
    let channels: Vec<_> = subscribes
        .iter()
        .map(|s| s["channel"].as_str().unwrap())
        .collect();
    assert_eq!(channels, ["user", "heartbeats"]);
    let tokens: Vec<_> = subscribes
        .iter()
        .map(|s| s["jwt"].as_str().unwrap().to_owned())
        .collect();
    assert!(tokens.iter().all(|t| t.starts_with("eyJ")));
    assert_ne!(tokens[0], tokens[1], "each subscribe is signed on its own");
    handle.stop();
}

#[test]
fn a_dropped_connection_reconnects_signs_again_and_says_so() {
    let rest = FakeCoinbase::start();
    let stream = FakeStream::start();
    let sink = Arc::new(Collector::default());
    let mut handle = connection(&rest, &stream)
        .open_stream(sink.clone())
        .unwrap();
    sink.wait_for("the first connection", |e| connected_count(e) == 1);
    let first_tokens: Vec<_> = stream
        .subscribes()
        .iter()
        .map(|s| s["jwt"].clone())
        .collect();

    stream.drop_connections();
    sink.wait_for("a disconnect", |e| {
        e.iter()
            .any(|e| matches!(e, StreamEvent::Disconnected { .. }))
    });
    sink.wait_for("the second connection", |e| connected_count(e) == 2);

    assert_eq!(stream.connections(), 2);
    let all: Vec<_> = stream
        .subscribes()
        .iter()
        .map(|s| s["jwt"].clone())
        .collect();
    assert_eq!(all.len(), 4, "user and heartbeats, twice");
    assert!(
        all[2..].iter().all(|t| !first_tokens.contains(t)),
        "tokens are never reused"
    );
    handle.stop();
}

#[test]
fn a_gap_in_sequence_numbers_is_reported() {
    let rest = FakeCoinbase::start();
    let stream = FakeStream::start();
    let sink = Arc::new(Collector::default());
    let mut handle = connection(&rest, &stream)
        .open_stream(sink.clone())
        .unwrap();
    sink.wait_for("connect", |e| connected_count(e) == 1);
    // The fake numbers the confirmation 1; skip ahead.
    let mut skipped: serde_json::Value = serde_json::from_str(&user_message(
        "update",
        &[user_order("o", "c", "OPEN", "0", "1")],
    ))
    .unwrap();
    skipped["sequence_num"] = serde_json::json!(50);
    stream.push(&skipped.to_string());
    sink.wait_for("a gap", |e| e.contains(&StreamEvent::Gap));
    handle.stop();
}

#[test]
fn a_feed_that_goes_silent_is_noticed_and_replaced() {
    let rest = FakeCoinbase::start();
    let stream = FakeStream::start();
    let sink = Arc::new(Collector::default());
    let mut handle = connection(&rest, &stream)
        .open_stream(sink.clone())
        .unwrap();
    sink.wait_for("connect", |e| connected_count(e) == 1);
    // The server stops sending heartbeats and data: the stream must give up on the socket itself.
    stream.go_silent(true);
    sink.wait_for("a heartbeat timeout", |e| {
        e.iter().any(
            |e| matches!(e, StreamEvent::Disconnected { reason } if reason.contains("timed out")),
        )
    });
    handle.stop();
}

#[test]
fn asking_for_a_reconnect_reconnects_at_once_and_a_stopped_stream_stays_stopped() {
    let rest = FakeCoinbase::start();
    let stream = FakeStream::start();
    let sink = Arc::new(Collector::default());
    let mut handle = connection(&rest, &stream)
        .open_stream(sink.clone())
        .unwrap();
    sink.wait_for("connect", |e| connected_count(e) == 1);
    handle.reconnect();
    sink.wait_for("the second connection", |e| connected_count(e) == 2);
    assert!(
        !sink
            .events()
            .iter()
            .any(|e| matches!(e, StreamEvent::Disconnected { .. })),
        "a requested reconnect is not a failure"
    );
    handle.stop();
    let connections = stream.connections();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        stream.connections(),
        connections,
        "no reconnects after stop"
    );
}

#[test]
fn nothing_is_sent_to_a_server_that_is_down_and_it_keeps_trying_with_backoff() {
    let rest = FakeCoinbase::start();
    let dead = {
        let stream = FakeStream::start();
        stream.url.clone()
    };
    let conn = Coinbase
        .connect(
            &credentials(&generate_key_pem()),
            &Env::default()
                .with_loopback_base_url(rest.url())
                .with_loopback_ws_url(dead)
                .with_stream_timing(StreamTiming::fast()),
        )
        .unwrap();
    let sink = Arc::new(Collector::default());
    let mut handle = conn.open_stream(sink.clone()).unwrap();
    sink.wait_for("two failed attempts", |e| {
        e.iter()
            .filter(|e| matches!(e, StreamEvent::Disconnected { .. }))
            .count()
            >= 2
    });
    handle.stop();
}

#[test]
fn a_server_error_that_echoes_the_subscription_is_scrubbed_and_short() {
    use super::fake::KEY_NAME;
    let rest = FakeCoinbase::start();
    let stream = FakeStream::start();
    let pem = generate_key_pem();
    let key_line = pem
        .lines()
        .map(str::trim)
        .find(|line| line.len() > 12 && !line.starts_with("-----"))
        .expect("a line of key material")
        .to_owned();
    let sink = Arc::new(Collector::default());
    let _handle = Coinbase
        .connect(
            &credentials(&pem),
            &Env::default()
                .with_loopback_base_url(rest.url())
                .with_loopback_ws_url(stream.url.clone())
                .with_stream_timing(StreamTiming::fast()),
        )
        .unwrap()
        .open_stream(sink.clone())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while stream.subscribes().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let subscribe = stream.subscribes().remove(0);
    let token = subscribe["jwt"]
        .as_str()
        .expect("a token was sent")
        .to_owned();

    // A server that answers a bad subscription by quoting it back, at length, with a newline.
    let echoed = format!(
        "invalid subscription {subscribe} for {KEY_NAME} key {key_line}\n{}",
        "x".repeat(3_000)
    );
    stream.push(&serde_json::json!({"type": "error", "message": echoed}).to_string());
    sink.wait_for("the disconnect", |e| {
        e.iter()
            .any(|e| matches!(e, StreamEvent::Disconnected { .. }))
    });
    let reason = sink
        .events()
        .into_iter()
        .find_map(|e| match e {
            StreamEvent::Disconnected { reason } => Some(reason),
            _ => None,
        })
        .unwrap();
    // A failure names which value leaked and shows the reason with every secret blanked out: the
    // test must not print the very values it is checking for.
    let secrets = [token.as_str(), KEY_NAME, key_line.as_str()];
    let shown = secrets.iter().fold(reason.clone(), |text, secret| {
        text.replace(secret, "<secret>")
    });
    for (index, secret) in secrets.iter().enumerate() {
        assert!(
            !reason.contains(secret),
            "secret #{index} leaked into the reason: {shown}"
        );
    }
    assert!(!reason.contains("eyJ"), "{reason}");
    assert!(!reason.contains('\n'));
    assert!(
        reason.chars().count() <= 260,
        "a server's words are cut short: {} characters",
        reason.chars().count()
    );
}
