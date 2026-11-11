//! A stateful stand-in for Coinbase's REST API, for tests. It keeps orders, honours the requests
//! this crate makes and replies in the shapes of the published specification. Behaviour switches
//! let a test model the failures that matter: an order accepted whose reply is lost, a refusal, a
//! rate limit.
//!
//! The shapes are **constructed from the specification, not captured from the live service.**

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};

use super::{FIELD_NAME, FIELD_PRIVATE_KEY};
use crate::secret::Credentials;
use crate::testing::mock::{MockServer, RecordedRequest, Reply};

pub const KEY_NAME: &str = "organizations/org-1/apiKeys/key-1";

/// A fresh P-256 signing key as the PEM Coinbase shows.
pub fn generate_key_pem() -> String {
    let rng = SystemRandom::new();
    let document = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
        .expect("generate a key");
    let outer = crate::auth::der::read(document.as_ref()).expect("pkcs8");
    let version = crate::auth::der::read(outer.content).expect("version");
    let algorithm = crate::auth::der::read(version.rest).expect("algorithm");
    let sec1 = crate::auth::der::read(algorithm.rest).expect("key").content;
    let body = STANDARD.encode(sec1);
    format!(
        "-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----\n",
        body.as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).expect("ascii"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

pub fn credentials(pem: &str) -> Credentials {
    Credentials::new()
        .with(FIELD_NAME, KEY_NAME)
        .with(FIELD_PRIVATE_KEY, pem)
}

/// The portfolio every fake belongs to unless a test says otherwise.
pub const DEFAULT_PORTFOLIO: &str = "pf-1";

/// What `/key_permissions` says for a key of `portfolio`.
fn permissions(portfolio: &str, can_trade: bool) -> String {
    format!(
        r#"{{"can_view":true,"can_trade":{can_trade},"can_transfer":false,"portfolio_uuid":"{portfolio}","portfolio_type":"DEFAULT"}}"#
    )
}

#[derive(Clone, Debug)]
pub struct FakeOrder {
    pub id: String,
    pub client_id: String,
    pub product: String,
    pub side: String,
    pub status: String,
    pub configuration: Value,
    pub filled_size: String,
    pub created_time: String,
}

#[derive(Default)]
pub struct State {
    pub orders: Vec<FakeOrder>,
    next_id: u32,
    /// Create the order, then drop the connection without replying.
    pub drop_reply_after_create: bool,
    /// Create the order, then answer success with a body that is cut off.
    pub garble_create_reply: bool,
    /// Refuse the next create with this `new_order_failure_reason`.
    pub refuse_create_with: Option<String>,
    /// Answer the next N requests with 429.
    pub rate_limit_next: usize,
    /// Answer every preview (order and edit) with a 422, as a broker that cannot preview would.
    pub fail_preview: bool,
    /// Answer the Nth (from 0) `batch_cancel` request with a 502.
    pub fail_cancel_batch: Option<usize>,
    /// How many `batch_cancel` requests were received.
    pub cancel_batches_seen: usize,
    /// What `cancel` says for each order, instead of success.
    pub cancel_failure: Option<String>,
    /// What `edit` says instead of success.
    pub edit_failure: Option<String>,
    /// Read-only mode: `can_trade` false in the key permissions.
    pub view_only_key: bool,
    /// The portfolio this key belongs to. Empty means [`DEFAULT_PORTFOLIO`]. A second fake with
    /// another portfolio is a second account: it holds its own orders and sees none of the first's.
    pub portfolio_uuid: String,
    /// Create the next order, then answer it with this HTTP status instead of success.
    pub status_after_create: Option<u16>,
    /// Answer the next order write (create, cancel or edit) with this HTTP status, doing nothing.
    pub status_for_next_write: Option<u16>,
    /// Leave the `time` out of the next best-bid-ask answer.
    pub quote_without_time: bool,
    /// Answer every lookup of one order by id with the 400 Coinbase's sandbox gives, which does
    /// not keep the orders it is sent.
    pub order_lookup_invalid: bool,
    /// Answer every request 401, as a revoked key would.
    pub revoked: bool,
    pub fills: Vec<Value>,
    pub requests_seen: Vec<String>,
}

pub struct FakeCoinbase {
    pub server: MockServer,
    pub state: Arc<Mutex<State>>,
}

fn order_json(order: &FakeOrder) -> Value {
    let config = order.configuration.clone();
    let (kind, tif) = match config
        .as_object()
        .and_then(|m| m.keys().next())
        .map(String::as_str)
    {
        Some(k) if k.starts_with("market") => ("MARKET", "IMMEDIATE_OR_CANCEL"),
        Some(k) if k.starts_with("stop_limit") => ("STOP_LIMIT", "GOOD_UNTIL_CANCELLED"),
        Some(k) if k.ends_with("gtd") => ("LIMIT", "GOOD_UNTIL_DATE_TIME"),
        Some(k) if k.ends_with("fok") => ("LIMIT", "FILL_OR_KILL"),
        _ => ("LIMIT", "GOOD_UNTIL_CANCELLED"),
    };
    json!({
        "order_id": order.id,
        "client_order_id": order.client_id,
        "product_id": order.product,
        "side": order.side,
        "status": order.status,
        "order_type": kind,
        "time_in_force": tif,
        "created_time": order.created_time,
        "filled_size": order.filled_size,
        "filled_value": "0",
        "average_filled_price": "0",
        "total_fees": "0",
        "size_in_quote": false,
        "order_configuration": config,
    })
}

impl FakeCoinbase {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        let server = MockServer::start(move |request| {
            let mut state = shared.lock().expect("fake state");
            state
                .requests_seen
                .push(format!("{} {}", request.method, request.path()));
            if state.revoked {
                return Reply::json(
                    401,
                    r#"{"message":"unauthenticated","code":"UNAUTHENTICATED"}"#,
                );
            }
            if state.rate_limit_next > 0 {
                state.rate_limit_next -= 1;
                return Reply::json(429, r#"{"message":"slow down"}"#)
                    .with_header("Retry-After", "1");
            }
            route(&mut state, request)
        });
        Self { server, state }
    }

    pub fn url(&self) -> &str {
        &self.server.url
    }

    pub fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().expect("fake state"))
    }

    /// Changes an order on the exchange (as if it traded or was cancelled) without telling any
    /// stream. Adds a fill row when the filled amount grows.
    pub fn set_order(&self, id: &str, status: &str, filled: &str) {
        self.with_state(|state| {
            let index = state
                .orders
                .iter()
                .position(|o| o.id == id)
                .expect("a known order");
            let (previous, side, product) = {
                let order = &mut state.orders[index];
                let previous = order.filled_size.clone();
                order.status = status.to_owned();
                order.filled_size = filled.to_owned();
                (previous, order.side.clone(), order.product.clone())
            };
            let before: rust_decimal::Decimal = previous.parse().unwrap_or_default();
            let after: rust_decimal::Decimal = filled.parse().unwrap_or_default();
            if after > before {
                let n = state.fills.len() + 1;
                state.fills.push(json!({
                    "trade_id": format!("trade-{n}"), "entry_id": format!("entry-{n}"),
                    "order_id": id, "product_id": product, "side": side,
                    "price": "90000.00", "size": (after - before).to_string(),
                    "commission": "0.10", "liquidity_indicator": "MAKER",
                    "trade_time": "2026-11-09T12:00:01Z",
                }));
            }
        });
    }

    pub fn posts_to_create(&self) -> usize {
        self.with_state(|s| {
            s.requests_seen
                .iter()
                .filter(|r| r.as_str() == "POST /api/v3/brokerage/orders")
                .count()
        })
    }
}

fn query_values<'a>(request: &'a RecordedRequest, key: &str) -> Vec<&'a str> {
    request
        .query()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| v)
        .collect()
}

fn route(state: &mut State, request: &RecordedRequest) -> Reply {
    let path = request
        .path()
        .strip_prefix("/api/v3/brokerage")
        .unwrap_or("");
    match (request.method.as_str(), path) {
        ("GET", "/key_permissions") => {
            let portfolio = if state.portfolio_uuid.is_empty() {
                DEFAULT_PORTFOLIO
            } else {
                state.portfolio_uuid.as_str()
            };
            Reply::json(200, permissions(portfolio, !state.view_only_key))
        }
        ("GET", "/accounts") => Reply::json(
            200,
            r#"{"accounts":[{"uuid":"a1","currency":"USD","available_balance":{"value":"10000.00","currency":"USD"},"hold":{"value":"0","currency":"USD"},"active":true}],"has_next":false,"cursor":""}"#,
        ),
        ("GET", "/products") => Reply::json(
            200,
            r#"{"products":[{"product_id":"BTC-USD","base_currency_id":"BTC","quote_currency_id":"USD","price_increment":"0.01","base_increment":"0.00000001","quote_increment":"0.01","base_min_size":"0.00001","base_max_size":"1000","quote_min_size":"1","quote_max_size":"1000000","status":"online","product_type":"SPOT"}]}"#,
        ),
        ("GET", "/best_bid_ask") if state.quote_without_time => Reply::json(
            200,
            r#"{"pricebooks":[{"product_id":"BTC-USD","bids":[{"price":"89999.50","size":"1"}],"asks":[{"price":"90000.50","size":"1"}]}]}"#,
        ),
        ("GET", "/best_bid_ask") => Reply::json(
            200,
            r#"{"pricebooks":[{"product_id":"BTC-USD","bids":[{"price":"89999.50","size":"1"}],"asks":[{"price":"90000.50","size":"1"}],"time":"2026-11-09T12:00:00Z"}]}"#,
        ),
        ("POST", "/orders" | "/orders/batch_cancel" | "/orders/edit")
            if state.status_for_next_write.is_some() =>
        {
            let status = state.status_for_next_write.take().unwrap_or(500);
            Reply::json(
                status,
                r#"{"error":"DUPLICATE_CLIENT_ORDER_ID","message":"conflict"}"#,
            )
        }
        ("GET", "/transaction_summary") => Reply::json(
            200,
            r#"{"fee_tier":{"pricing_tier":"Advanced 1","maker_fee_rate":"0.004","taker_fee_rate":"0.006"}}"#,
        ),
        ("POST", "/orders/preview" | "/orders/edit_preview") if state.fail_preview => Reply::json(
            422,
            r#"{"message":"preview is unavailable","code":"UNAVAILABLE"}"#,
        ),
        ("POST", "/orders/preview") => {
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let size = body["order_configuration"]
                .as_object()
                .and_then(|m| m.values().next())
                .and_then(|c| c["base_size"].as_str())
                .unwrap_or("0");
            if size == "0.000000001" || size == "99999" {
                Reply::json(
                    200,
                    r#"{"errs":["PREVIEW_INSUFFICIENT_FUND"],"warning":[]}"#,
                )
            } else {
                Reply::json(
                    200,
                    r#"{"order_total":"90.54","commission_total":"0.54","errs":[],"warning":["BIG_ORDER"],"base_size":"0.001","quote_size":"90.00","best_bid":"89999.50","best_ask":"90000.50","slippage":"0.0001","est_average_filled_price":"90000.00"}"#,
                )
            }
        }
        ("POST", "/orders") => {
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let client_id = body["client_order_id"].as_str().unwrap_or("").to_owned();
            if let Some(reason) = state.refuse_create_with.take() {
                return Reply::json(
                    200,
                    json!({"success": false, "error_response": {
                        "error": "UNKNOWN_FAILURE_REASON", "message": "refused",
                        "new_order_failure_reason": reason}})
                    .to_string(),
                );
            }
            if let Some(existing) = state.orders.iter().find(|o| o.client_id == client_id) {
                return Reply::json(
                    200,
                    json!({"success": false, "error_response": {
                        "error": "UNKNOWN_FAILURE_REASON", "message": "duplicate",
                        "new_order_failure_reason": "DUPLICATE_CLIENT_ORDER_ID"}, "existing": existing.id})
                    .to_string(),
                );
            }
            state.next_id += 1;
            let order = FakeOrder {
                id: format!("order-{}", state.next_id),
                client_id,
                product: body["product_id"].as_str().unwrap_or("").to_owned(),
                side: body["side"].as_str().unwrap_or("").to_owned(),
                status: "OPEN".to_owned(),
                configuration: body["order_configuration"].clone(),
                filled_size: "0".to_owned(),
                created_time: "2026-11-09T12:00:00.000Z".to_owned(),
            };
            let reply = json!({"success": true, "success_response": {
                "order_id": order.id, "product_id": order.product,
                "side": order.side, "client_order_id": order.client_id}});
            state.orders.push(order);
            if let Some(status) = state.status_after_create.take() {
                Reply::json(
                    status,
                    r#"{"error":"DUPLICATE_CLIENT_ORDER_ID","message":"conflict"}"#,
                )
            } else if state.drop_reply_after_create {
                state.drop_reply_after_create = false;
                Reply::Drop
            } else if state.garble_create_reply {
                state.garble_create_reply = false;
                Reply::json(200, r#"{"success":true,"success_resp"#)
            } else {
                Reply::json(200, reply.to_string())
            }
        }
        ("POST", "/orders/batch_cancel") => {
            let batch = state.cancel_batches_seen;
            state.cancel_batches_seen += 1;
            if state.fail_cancel_batch == Some(batch) {
                return Reply::json(502, r#"{"message":"bad gateway"}"#);
            }
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            let ids: Vec<String> = body["order_ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let failure = state.cancel_failure.clone();
            let results: Vec<Value> = ids
                .iter()
                .map(|id| match (&failure, state.orders.iter_mut().find(|o| &o.id == id)) {
                    (Some(reason), _) => json!({"success": false, "failure_reason": reason, "order_id": id}),
                    (None, Some(order)) => {
                        order.status = "CANCEL_QUEUED".to_owned();
                        json!({"success": true, "failure_reason": "UNKNOWN_CANCEL_FAILURE_REASON", "order_id": id})
                    }
                    (None, None) => json!({"success": false, "failure_reason": "UNKNOWN_CANCEL_ORDER", "order_id": id}),
                })
                .collect();
            Reply::json(200, json!({ "results": results }).to_string())
        }
        ("POST", "/orders/edit_preview") => match &state.edit_failure {
            Some(reason) => Reply::json(
                200,
                json!({"errors": [{"edit_failure_reason": reason}]}).to_string(),
            ),
            None => Reply::json(
                200,
                r#"{"errors":[],"order_total":"89.99","commission_total":"0.54","base_size":"0.001","quote_size":"89.45"}"#,
            ),
        },
        ("POST", "/orders/edit") => {
            let body: Value = serde_json::from_str(&request.body).unwrap_or(Value::Null);
            if let Some(reason) = &state.edit_failure {
                return Reply::json(
                    200,
                    json!({"success": false, "errors": [{"edit_failure_reason": reason}]})
                        .to_string(),
                );
            }
            let id = body["order_id"].as_str().unwrap_or("");
            if let Some(order) = state.orders.iter_mut().find(|o| o.id == id) {
                order.status = "EDIT_QUEUED".to_owned();
                if let Some(config) = order.configuration.as_object_mut()
                    && let Some(inner) = config.values_mut().next()
                {
                    inner["limit_price"] = body["price"].clone();
                    inner["base_size"] = body["size"].clone();
                }
            }
            Reply::json(200, r#"{"success":true,"errors":[]}"#)
        }
        ("GET", "/orders/historical/batch") => {
            let wanted = query_values(request, "order_status");
            // Coinbase's own rule, met on the first real run: active statuses are asked for one
            // at a time, never together or mixed with others.
            const ACTIVE: [&str; 5] = ["OPEN", "PENDING", "QUEUED", "EDIT_QUEUED", "CANCEL_QUEUED"];
            if wanted.len() > 1 && wanted.iter().any(|status| ACTIVE.contains(status)) {
                return Reply::json(
                    400,
                    r#"{"error":"INVALID_ARGUMENT","message":"Cannot pass multiple statuses when requesting active orders (OPEN, PENDING, QUEUED, EDIT_QUEUED, or CANCEL_QUEUED)","error_details":""}"#,
                );
            }
            let orders: Vec<Value> = state
                .orders
                .iter()
                .filter(|o| wanted.is_empty() || wanted.contains(&o.status.as_str()))
                .map(order_json)
                .collect();
            Reply::json(
                200,
                json!({"orders": orders, "has_next": false, "cursor": ""}).to_string(),
            )
        }
        ("GET", "/orders/historical/fills") => {
            Reply::json(200, json!({"fills": state.fills, "cursor": ""}).to_string())
        }
        ("GET", other) if other.starts_with("/orders/historical/") => {
            let id = other.trim_start_matches("/orders/historical/");
            if state.order_lookup_invalid {
                return Reply::json(
                    400,
                    r#"{"error":"INVALID_ARGUMENT","message":"invalid OrderId {order_id} passed to endpoint","error_details":""}"#,
                );
            }
            match state.orders.iter().find(|o| o.id == id) {
                Some(order) => Reply::json(200, json!({ "order": order_json(order) }).to_string()),
                None => Reply::json(404, r#"{"message":"order not found","error":"NOT_FOUND"}"#),
            }
        }
        _ => Reply::json(404, r#"{"message":"no such route"}"#),
    }
}

// ---- The user WebSocket --------------------------------------------------------------------------

use std::collections::VecDeque;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

/// A fake of `advanced-trade-ws-user.coinbase.com`: accepts connections, records each subscribe,
/// confirms it, sends the configured snapshot, then relays whatever a test pushes.
pub struct FakeStream {
    pub url: String,
    inner: Arc<StreamInner>,
    thread: Option<JoinHandle<()>>,
    address: String,
}

#[derive(Default)]
struct StreamInner {
    stop: AtomicBool,
    /// Bumped to make every connection close.
    generation: AtomicU64,
    connections: AtomicU64,
    subscribes: Mutex<Vec<Value>>,
    snapshot: Mutex<Option<String>>,
    outbox: Mutex<VecDeque<String>>,
    /// Send nothing at all after subscribing (a dead feed).
    silent: AtomicBool,
}

impl FakeStream {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let address = listener.local_addr().expect("address").to_string();
        let inner = Arc::new(StreamInner::default());
        let shared = Arc::clone(&inner);
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if shared.stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || serve_socket(stream, &shared));
            }
        });
        Self {
            url: format!("ws://{address}"),
            inner,
            thread: Some(thread),
            address,
        }
    }

    /// The messages a client sends to subscribe (channel and whether it carried a token), in order.
    pub fn subscribes(&self) -> Vec<Value> {
        self.inner.subscribes.lock().expect("subscribes").clone()
    }

    pub fn connections(&self) -> u64 {
        self.inner.connections.load(Ordering::SeqCst)
    }

    /// The text sent as the `user` channel's first message after every subscribe.
    pub fn set_snapshot(&self, text: &str) {
        *self.inner.snapshot.lock().expect("snapshot") = Some(text.to_owned());
    }

    /// Queues a message for every open connection.
    pub fn push(&self, text: &str) {
        self.inner
            .outbox
            .lock()
            .expect("outbox")
            .push_back(text.to_owned());
    }

    /// Closes every open connection (the client should reconnect).
    pub fn drop_connections(&self) {
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// From now on, say nothing after subscribing.
    pub fn go_silent(&self, silent: bool) {
        self.inner.silent.store(silent, Ordering::SeqCst);
    }
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(&self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_socket(stream: std::net::TcpStream, inner: &StreamInner) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(20)));
    let Ok(mut socket) = tungstenite::accept(stream) else {
        return;
    };
    inner.connections.fetch_add(1, Ordering::SeqCst);
    let generation = inner.generation.load(Ordering::SeqCst);
    let mut sequence = 0_u64;
    let mut subscribed_user = false;
    let mut beats = 0_u64;
    let mut last_beat = std::time::Instant::now();
    loop {
        if inner.stop.load(Ordering::SeqCst)
            || inner.generation.load(Ordering::SeqCst) != generation
        {
            let _ = socket.close(None);
            let _ = socket.flush();
            return;
        }
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                if let Ok(value) = serde_json::from_str::<Value>(text.as_str()) {
                    inner
                        .subscribes
                        .lock()
                        .expect("subscribes")
                        .push(value.clone());
                    if inner.silent.load(Ordering::SeqCst) {
                        continue;
                    }
                    if value["channel"] == "user" {
                        subscribed_user = true;
                        sequence += 1;
                        let confirmation = json!({"channel": "subscriptions", "sequence_num": sequence,
                            "events": [{"subscriptions": {"user": []}}]});
                        let _ = socket.send(tungstenite::Message::text(confirmation.to_string()));
                        if let Some(snapshot) = inner.snapshot.lock().expect("snapshot").clone() {
                            sequence += 1;
                            let _ = socket.send(tungstenite::Message::text(with_sequence(
                                &snapshot, sequence,
                            )));
                        }
                    }
                }
            }
            Ok(tungstenite::Message::Close(_)) => return,
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return,
        }
        if subscribed_user
            && !inner.silent.load(Ordering::SeqCst)
            && last_beat.elapsed() >= std::time::Duration::from_millis(100)
        {
            // The real server beats once a second; a test does not wait that long.
            last_beat = std::time::Instant::now();
            beats += 1;
            sequence += 1;
            let beat = json!({"channel": "heartbeats", "sequence_num": sequence,
                "events": [{"current_time": "2026-11-09 12:00:00", "heartbeat_counter": beats}]});
            if socket
                .send(tungstenite::Message::text(beat.to_string()))
                .is_err()
            {
                return;
            }
        }
        if subscribed_user && !inner.silent.load(Ordering::SeqCst) {
            let next = inner.outbox.lock().expect("outbox").pop_front();
            if let Some(text) = next {
                sequence += 1;
                if socket
                    .send(tungstenite::Message::text(with_sequence(&text, sequence)))
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// Stamps the next sequence number onto a message, unless the test already chose one.
fn with_sequence(text: &str, sequence: u64) -> String {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(mut map)) => {
            map.entry("sequence_num").or_insert(json!(sequence));
            Value::Object(map).to_string()
        }
        _ => text.to_owned(),
    }
}

/// One `user` channel order in the shape of the published AsyncAPI document.
pub fn user_order(
    order_id: &str,
    client_id: &str,
    status: &str,
    filled: &str,
    leaves: &str,
) -> Value {
    json!({
        "order_id": order_id, "client_order_id": client_id, "product_id": "BTC-USD",
        "order_side": "BUY", "order_type": "LIMIT", "status": status,
        "cumulative_quantity": filled, "leaves_quantity": leaves, "avg_price": "0",
        "limit_price": "90000.00", "stop_price": "0", "filled_value": "0", "total_fees": "0",
        "number_of_fills": "0", "post_only": "false", "time_in_force": "GOOD_UNTIL_CANCELLED",
        "creation_time": "2026-11-09T12:00:00.100Z", "end_time": "0001-01-01T00:00:00Z",
        "product_type": "SPOT", "reject_reason": "REJECT_REASON_UNSPECIFIED",
    })
}

/// A `user` channel message holding `orders` as a `snapshot` or an `update`.
pub fn user_message(kind: &str, orders: &[Value]) -> String {
    json!({"channel": "user", "timestamp": "2026-11-09T12:00:00Z",
        "events": [{"type": kind, "orders": orders}]})
    .to_string()
}

/// The fake REST and stream together, as the conformance suite drives them.
pub struct CoinbaseExchange {
    pub rest: FakeCoinbase,
    pub stream: FakeStream,
}

impl CoinbaseExchange {
    pub fn start() -> Self {
        Self {
            rest: FakeCoinbase::start(),
            stream: FakeStream::start(),
        }
    }
}

impl crate::testing::conformance::Exchange for CoinbaseExchange {
    fn broker(&self) -> &'static dyn crate::adapter::Broker {
        &super::Coinbase
    }

    fn credentials(&self) -> Credentials {
        credentials(&generate_key_pem())
    }

    fn account(&self) -> crate::model::types::AccountId {
        let portfolio = self.rest.with_state(|s| s.portfolio_uuid.clone());
        crate::model::types::AccountId::new(if portfolio.is_empty() {
            DEFAULT_PORTFOLIO.to_owned()
        } else {
            portfolio
        })
    }

    fn env(&self, clock: Arc<dyn crate::adapter::Clock>) -> crate::adapter::Env {
        crate::adapter::Env::default()
            .with_clock(clock)
            .with_loopback_base_url(self.rest.url())
            .with_loopback_ws_url(self.stream.url.clone())
            .with_stream_timing(crate::adapter::StreamTiming::fast())
            .with_request_gap(std::time::Duration::from_millis(2))
    }

    fn draft(&self) -> crate::model::types::OrderDraft {
        use crate::model::types::{OrderDraft, OrderKind, ProductId, Side, TimeInForce};
        OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Buy,
            kind: OrderKind::Limit {
                base_size: "0.001".parse().expect("decimal"),
                price: "90000.00".parse().expect("decimal"),
                tif: TimeInForce::GoodTilCancelled,
                post_only: false,
            },
        }
    }

    fn off_increment_draft(&self) -> crate::model::types::OrderDraft {
        use crate::model::types::OrderKind;
        let mut draft = self.draft();
        if let OrderKind::Limit { price, .. } = &mut draft.kind {
            *price = "90000.005".parse().expect("decimal");
        }
        draft
    }

    fn lose_next_order_reply(&self) {
        self.rest.with_state(|s| s.drop_reply_after_create = true);
    }

    fn refuse_next_order(&self) {
        self.rest
            .with_state(|s| s.refuse_create_with = Some("INSUFFICIENT_FUND".to_owned()));
    }

    fn orders_created(&self) -> usize {
        self.rest.with_state(|s| s.orders.len())
    }

    fn order_posts(&self) -> usize {
        self.rest.posts_to_create()
    }

    fn order_ids(&self) -> Vec<String> {
        self.rest
            .with_state(|s| s.orders.iter().map(|o| o.id.clone()).collect())
    }

    fn change_order(&self, id: &str, status: &str, filled: &str, announce: bool) {
        self.rest.set_order(id, status, filled);
        if announce {
            let (client_id, leaves) = self.rest.with_state(|s| {
                let order = s.orders.iter().find(|o| o.id == id).expect("known order");
                let filled: rust_decimal::Decimal = order.filled_size.parse().unwrap_or_default();
                (
                    order.client_id.clone(),
                    (rust_decimal::Decimal::new(1, 3) - filled).to_string(),
                )
            });
            self.stream.push(&user_message(
                "update",
                &[user_order(id, &client_id, status, filled, &leaves)],
            ));
        }
    }

    fn announce_again(&self, id: &str, status: &str, filled: &str) {
        let client_id = self.rest.with_state(|s| {
            s.orders
                .iter()
                .find(|o| o.id == id)
                .map(|o| o.client_id.clone())
                .unwrap_or_default()
        });
        let leaves = (rust_decimal::Decimal::new(1, 3)
            - filled.parse::<rust_decimal::Decimal>().unwrap_or_default())
        .to_string();
        self.stream.push(&user_message(
            "update",
            &[user_order(id, &client_id, status, filled, &leaves)],
        ));
    }

    fn cancel_will_fail_because_filled(&self) {
        self.rest
            .with_state(|s| s.cancel_failure = Some("ORDER_IS_FULLY_FILLED".to_owned()));
    }

    fn drop_stream(&self) {
        self.stream.drop_connections();
    }

    fn stream_connections(&self) -> u64 {
        self.stream.connections()
    }

    fn revoke_key(&self) {
        self.rest.with_state(|s| s.revoked = true);
    }

    fn rate_limit_next(&self, requests: usize) {
        self.rest.with_state(|s| s.rate_limit_next = requests);
    }
}
