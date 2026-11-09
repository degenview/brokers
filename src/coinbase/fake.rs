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

pub const PERMISSIONS: &str = r#"{"can_view":true,"can_trade":true,"can_transfer":false,"portfolio_uuid":"pf-1","portfolio_type":"DEFAULT"}"#;

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
    /// Refuse the next create with this `new_order_failure_reason`.
    pub refuse_create_with: Option<String>,
    /// Answer the next N requests with 429.
    pub rate_limit_next: usize,
    /// What `cancel` says for each order, instead of success.
    pub cancel_failure: Option<String>,
    /// What `edit` says instead of success.
    pub edit_failure: Option<String>,
    /// Read-only mode: `can_trade` false in the key permissions.
    pub view_only_key: bool,
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
            if state.view_only_key {
                Reply::json(
                    200,
                    r#"{"can_view":true,"can_trade":false,"can_transfer":false,"portfolio_uuid":"pf-1","portfolio_type":"DEFAULT"}"#,
                )
            } else {
                Reply::json(200, PERMISSIONS)
            }
        }
        ("GET", "/accounts") => Reply::json(
            200,
            r#"{"accounts":[{"uuid":"a1","currency":"USD","available_balance":{"value":"10000.00","currency":"USD"},"hold":{"value":"0","currency":"USD"},"active":true}],"has_next":false,"cursor":""}"#,
        ),
        ("GET", "/products") => Reply::json(
            200,
            r#"{"products":[{"product_id":"BTC-USD","base_currency_id":"BTC","quote_currency_id":"USD","price_increment":"0.01","base_increment":"0.00000001","quote_increment":"0.01","base_min_size":"0.00001","base_max_size":"1000","quote_min_size":"1","quote_max_size":"1000000","status":"online","product_type":"SPOT"}]}"#,
        ),
        ("GET", "/best_bid_ask") => Reply::json(
            200,
            r#"{"pricebooks":[{"product_id":"BTC-USD","bids":[{"price":"89999.50","size":"1"}],"asks":[{"price":"90000.50","size":"1"}],"time":"2026-11-09T12:00:00Z"}]}"#,
        ),
        ("GET", "/transaction_summary") => Reply::json(
            200,
            r#"{"fee_tier":{"pricing_tier":"Advanced 1","maker_fee_rate":"0.004","taker_fee_rate":"0.006"}}"#,
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
            if state.drop_reply_after_create {
                state.drop_reply_after_create = false;
                Reply::Drop
            } else {
                Reply::json(200, reply.to_string())
            }
        }
        ("POST", "/orders/batch_cancel") => {
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
        ("GET", "/orders/historical/fills") => Reply::json(200, r#"{"fills":[],"cursor":""}"#),
        ("GET", other) if other.starts_with("/orders/historical/") => {
            let id = other.trim_start_matches("/orders/historical/");
            match state.orders.iter().find(|o| o.id == id) {
                Some(order) => Reply::json(200, json!({ "order": order_json(order) }).to_string()),
                None => Reply::json(404, r#"{"message":"order not found","error":"NOT_FOUND"}"#),
            }
        }
        _ => Reply::json(404, r#"{"message":"no such route"}"#),
    }
}
