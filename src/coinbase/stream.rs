//! The Coinbase `user` WebSocket channel: live order updates.
//!
//! Host `wss://advanced-trade-ws-user.coinbase.com`. A subscription is a JSON message carrying a
//! JWT, sent within five seconds of connecting; the server then sends a `subscriptions`
//! confirmation, a `snapshot` of open orders (in batches of 50) and `update`s, and a `heartbeats`
//! message every second if subscribed to that channel. Every message carries a `sequence_num`.
//! Unknown message types may appear at any time and are ignored.
//!
//! A token is signed freshly for every subscribe, including after every reconnect: they live two
//! minutes. Whether an already-open socket stays authorised after its token expires is not
//! documented, so nothing here relies on one socket living long; and whether `sequence_num` counts
//! per connection or per product is also unverified, so a gap only triggers a re-read of state
//! (harmless when it is a false alarm).

use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tungstenite::Message;

use super::jwt::JwtSigner;
use super::mapping::order_from_user;
use crate::adapter::{Clock, StreamHandle, StreamTiming};
use crate::model::events::{EventSink, StreamEvent};
use crate::scrub::Scrubber;
use crate::ws::{self, Backoff, Health};

pub(super) const USER_HOST: &str = "wss://advanced-trade-ws-user.coinbase.com";

/// The longest reason that is shown for a dropped connection.
const REASON_LIMIT: usize = 200;

/// Text that came from the server or the network, made safe to show and to log: credentials and
/// tokens removed (the server may echo the subscription it was sent), control characters dropped,
/// and cut short.
fn safe_reason(scrubber: &Scrubber, text: &str) -> String {
    let cleaned: String = scrubber
        .scrub(text)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.chars().count() > REASON_LIMIT {
        let mut cut: String = cleaned.chars().take(REASON_LIMIT).collect();
        cut.push('…');
        cut
    } else {
        cleaned
    }
}

pub(super) struct Config {
    pub url: String,
    pub scrubber: Scrubber,
    pub signer: Arc<JwtSigner>,
    pub clock: Arc<dyn Clock>,
    pub timing: StreamTiming,
}

pub(super) fn spawn(config: Config, sink: Arc<dyn EventSink>) -> StreamHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let reconnect = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = Arc::clone(&stop);
        let reconnect = Arc::clone(&reconnect);
        std::thread::Builder::new()
            .name("coinbase-user-stream".to_owned())
            .spawn(move || run(&config, sink.as_ref(), &stop, &reconnect))
            .expect("spawn the stream thread")
    };
    StreamHandle::new(stop, reconnect, thread)
}

fn run(config: &Config, sink: &dyn EventSink, stop: &AtomicBool, reconnect: &AtomicBool) {
    let mut backoff = Backoff::with(config.timing.backoff_initial, config.timing.backoff_max);
    while !stop.load(Ordering::Acquire) {
        match session(config, sink, stop, reconnect, &mut backoff) {
            Ended::Stopped => return,
            // A requested reconnect starts again at once.
            Ended::Reconnect => continue,
            Ended::Failed(reason) => {
                // The one place a reason leaves this module, whatever produced it.
                sink.event(StreamEvent::Disconnected {
                    reason: safe_reason(&config.scrubber, &reason),
                });
                backoff.wait(stop);
            }
        }
    }
}

enum Ended {
    Stopped,
    Reconnect,
    Failed(String),
}

fn subscribe_message(signer: &JwtSigner, channel: &str) -> Result<String, String> {
    let jwt = signer
        .sign_websocket()
        .map_err(|error| format!("could not sign the subscription: {error}"))?;
    Ok(json!({"type": "subscribe", "channel": channel, "jwt": jwt}).to_string())
}

fn session(
    config: &Config,
    sink: &dyn EventSink,
    stop: &AtomicBool,
    reconnect: &AtomicBool,
    backoff: &mut Backoff,
) -> Ended {
    let mut socket = match ws::connect(&config.url, config.timing.read_timeout) {
        Ok(socket) => socket,
        Err(error) => return Ended::Failed(format!("could not connect: {error}")),
    };
    // Subscribe at once: the server drops a socket that does not within five seconds.
    for channel in ["user", "heartbeats"] {
        let message = match subscribe_message(&config.signer, channel) {
            Ok(message) => message,
            Err(error) => return Ended::Failed(error),
        };
        if let Err(error) = socket.send(Message::text(message)) {
            return Ended::Failed(format!("could not subscribe: {error}"));
        }
    }
    let mut health = Health::new(config.timing.max_silence)
        .with_ping(config.timing.max_silence / 2, config.timing.max_silence / 2);
    let mut connected = false;
    let mut last_sequence: Option<u64> = None;
    loop {
        if stop.load(Ordering::Acquire) {
            let _ = socket.close(None);
            return Ended::Stopped;
        }
        if reconnect.swap(false, Ordering::AcqRel) {
            let _ = socket.close(None);
            return Ended::Reconnect;
        }
        let message = match health.read(&mut socket) {
            Ok(Some(message)) => message,
            Ok(None) => continue,
            Err(error) => return Ended::Failed(format!("the connection failed: {error}")),
        };
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => {
                return Ended::Failed("the server closed the connection".to_owned());
            }
            _ => continue,
        };
        let Ok(value) = serde_json::from_str::<Value>(text.as_str()) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("error") {
            let reason = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("the server reported an error");
            return Ended::Failed(format!("the server refused the subscription: {reason}"));
        }
        // A sequence number that jumps means messages were lost.
        if let Some(sequence) = value.get("sequence_num").and_then(Value::as_u64) {
            if last_sequence.is_some_and(|last| sequence > last + 1) {
                sink.event(StreamEvent::Gap);
            }
            last_sequence = Some(sequence);
        }
        match value.get("channel").and_then(Value::as_str) {
            Some("subscriptions") | Some("user") if !connected => {
                connected = true;
                backoff.reset();
                sink.event(StreamEvent::Connected);
            }
            _ => {}
        }
        if value.get("channel").and_then(Value::as_str) == Some("user") {
            handle_user(&value, config.clock.as_ref(), sink);
        }
    }
}

fn handle_user(message: &Value, clock: &dyn Clock, sink: &dyn EventSink) {
    let Some(events) = message.get("events").and_then(Value::as_array) else {
        return;
    };
    let now = clock.now_ms();
    for event in events {
        let snapshot = event.get("type").and_then(Value::as_str) == Some("snapshot");
        let orders: Vec<_> = event
            .get("orders")
            .and_then(Value::as_array)
            .map(|orders| {
                orders
                    .iter()
                    .filter_map(|order| order_from_user(order, now))
                    .collect()
            })
            .unwrap_or_default();
        if !orders.is_empty() {
            sink.event(StreamEvent::Orders { orders, snapshot });
        }
    }
}
