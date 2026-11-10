//! No credential, token or `Authorization` header may reach anything that leaves the adapter: an
//! error, its detail, its code, a `Debug` print, or a runtime snapshot. The servers here echo
//! everything they were sent, which is the worst a broker (or a proxy) could do.

use std::sync::Arc;

use super::Coinbase;
use super::fake::{KEY_NAME, credentials, generate_key_pem};
use crate::adapter::{Broker, Env};
use crate::model::pending::MemoryJournal;
use crate::model::reducer::Notice;
use crate::model::types::{ClientOrderId, OrderDraft, OrderKind, ProductId, Side, TimeInForce};
use crate::runtime::{BrokerRuntime, NoticeSink, RuntimeConfig, Status, Waker};
use crate::testing::mock::{MockServer, Reply};

/// Everything that must never appear: the key name, every line of the key, and any token.
fn secrets(pem: &str) -> Vec<String> {
    let mut found = vec![KEY_NAME.to_owned()];
    found.extend(
        pem.lines()
            .map(str::trim)
            .filter(|line| line.len() > 12 && !line.starts_with("-----"))
            .map(str::to_owned),
    );
    found
}

fn assert_clean(text: &str, secrets: &[String], context: &str) {
    for secret in secrets {
        assert!(
            !text.contains(secret.as_str()),
            "{context} leaked a credential: {text}"
        );
    }
    assert!(!text.contains("eyJ"), "{context} leaked a token: {text}");
    assert!(
        !text.to_lowercase().contains("bearer ey"),
        "{context} leaked a header: {text}"
    );
}

/// A server that answers every request with `status` and a body quoting the request back.
fn echo_server(status: u16) -> MockServer {
    MockServer::start(move |request| {
        let echoed = format!(
            "{} {} authorization={} body={}",
            request.method,
            request.target,
            request.header("authorization").unwrap_or(""),
            request.body
        );
        Reply::json(
            status,
            serde_json::json!({"message": echoed, "error": echoed, "code": echoed}).to_string(),
        )
    })
}

fn draft() -> OrderDraft {
    OrderDraft {
        product: ProductId::new("BTC-USD"),
        side: Side::Buy,
        kind: OrderKind::Limit {
            base_size: "0.001".parse().unwrap(),
            price: "90000.00".parse().unwrap(),
            tif: TimeInForce::GoodTilCancelled,
            post_only: false,
        },
    }
}

#[test]
fn errors_from_an_echoing_server_carry_no_credential() {
    let pem = generate_key_pem();
    let secrets = secrets(&pem);
    for status in [400_u16, 401, 403, 404, 429, 500, 502] {
        let server = echo_server(status);
        let env = Env::default().with_loopback_base_url(server.url.clone());
        let error = Coinbase
            .connect(&credentials(&pem), &env)
            .err()
            .expect("connect fails");
        let everything = format!("{error} | {} | {:?} | {error:?}", error.detail, error.code);
        assert_clean(&everything, &secrets, &format!("a {status} on connect"));
    }
}

#[test]
fn a_refused_order_and_a_failed_read_carry_no_credential() {
    let pem = generate_key_pem();
    let secrets = secrets(&pem);
    // Healthy enough to connect, hostile on everything else.
    let server = MockServer::start(|request| {
        if request.path().ends_with("/key_permissions") {
            return Reply::json(
                200,
                r#"{"can_view":true,"can_trade":true,"portfolio_uuid":"pf"}"#,
            );
        }
        let echoed = format!(
            "{} authorization={} body={}",
            request.target,
            request.header("authorization").unwrap_or(""),
            request.body
        );
        Reply::json(
            400,
            serde_json::json!({"message": echoed, "code": echoed}).to_string(),
        )
    });
    let env = Env::default().with_loopback_base_url(server.url.clone());
    let connection = Coinbase.connect(&credentials(&pem), &env).expect("connect");
    for error in [
        connection
            .place(&draft(), &ClientOrderId::new("c"))
            .unwrap_err(),
        connection.balances().unwrap_err(),
        connection.open_orders().unwrap_err(),
        connection.preview(&draft()).unwrap_err(),
    ] {
        let everything = format!("{error} | {} | {:?} | {error:?}", error.detail, error.code);
        assert_clean(&everything, &secrets, "a hostile 400");
    }
}

struct Quiet;
impl Waker for Quiet {
    fn wake(&self) {}
}
impl NoticeSink for Quiet {
    fn notice(&self, _: Notice) {}
}

#[test]
fn a_runtime_that_loses_its_key_shows_no_credential_anywhere() {
    let pem = generate_key_pem();
    let secrets = secrets(&pem);
    let server = echo_server(401);
    let env = Env::default()
        .with_loopback_base_url(server.url.clone())
        .with_request_gap(std::time::Duration::from_millis(1));
    let config = RuntimeConfig::new(
        env,
        Arc::new(MemoryJournal::new()),
        Arc::new(Quiet),
        Arc::new(Quiet),
    )
    .fast();
    let runtime = BrokerRuntime::start(&Coinbase, credentials(&pem), config);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !matches!(runtime.snapshot().status, Status::Failed(_)) {
        assert!(
            std::time::Instant::now() < deadline,
            "the runtime should fail on a 401"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let snapshot = runtime.snapshot();
    assert_clean(&format!("{snapshot:?}"), &secrets, "the snapshot");
    assert_clean(
        &snapshot.last_error.clone().unwrap_or_default(),
        &secrets,
        "last_error",
    );
    runtime.stop();
}

#[test]
fn credentials_and_the_signer_print_nothing_secret() {
    let pem = generate_key_pem();
    let creds = credentials(&pem);
    let shown = format!("{creds:?}");
    assert_clean(&shown, &secrets(&pem), "Credentials");
    let signer = super::jwt::JwtSigner::new(
        KEY_NAME,
        &pem,
        Arc::new(crate::testing::ManualClock::new(0)),
    )
    .unwrap();
    assert_clean(&format!("{signer:?}"), &secrets(&pem), "JwtSigner");
}
