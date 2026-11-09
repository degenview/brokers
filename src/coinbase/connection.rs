//! One authenticated Coinbase session.

use rust_decimal::Decimal;
use std::sync::Arc;

use super::descriptor::{DESCRIPTOR, FIELD_NAME, FIELD_PRIVATE_KEY, ID};
use super::jwt::JwtSigner;
use super::stream::{self, USER_HOST};
use super::time::parse_rfc3339_ms;
use super::wire::{
    AccountsPage, BestBidAsk, KeyPermissions, ProductDto, ProductsPage, parse_decimal,
};
use super::{BASE_URL, PREFIX};
use crate::adapter::{Clock, Connection, Env, Session, StreamHandle, StreamTiming};
use crate::auth::RequestSigner;
use crate::error::{BrokerError, ErrorKind, Result};
use crate::http::HttpClient;
use crate::model::capabilities::{Capabilities, Tradability};
use crate::model::events::EventSink;
use crate::model::orders::{
    CancelResult, EditOutcome, EditPreview, FeeSummary, FillFilter, HistoryFilter, OrderChange,
    OrderPreview, Page, PlacedOrder,
};
use crate::model::types::{
    AccountId, AccountKind, AccountRef, Balance, BrokerId, ClientOrderId, LiveFill, LiveOrder,
    OrderDraft, OrderId, OrderType, Permissions, ProductId, ProductSpec, Quote, Restrictions,
    TifKind,
};
use crate::scrub::Scrubber;
use crate::secret::Credentials;

/// Products are read in pages of this many.
const PRODUCT_PAGE: usize = 1000;
/// Accounts are read in pages of this many (the documented maximum).
const ACCOUNT_PAGE: usize = 250;
/// A guard against a cursor that never ends.
const MAX_PAGES: usize = 40;

pub struct CoinbaseConnection {
    pub(super) http: HttpClient,
    session: Session,
    pub(super) clock: Arc<dyn Clock>,
    signer: Arc<JwtSigner>,
    ws_url: String,
    timing: StreamTiming,
}

pub(super) fn path(suffix: &str) -> String {
    format!("{PREFIX}{suffix}")
}

pub(super) fn unreadable(what: &str) -> BrokerError {
    BrokerError::new(
        ErrorKind::Invalid,
        format!("Coinbase sent {what} that could not be read."),
    )
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
    what: &str,
) -> Result<T> {
    serde_json::from_value(value).map_err(|_| unreadable(what))
}

impl CoinbaseConnection {
    /// Checks the key and reads what it may do. Reads only.
    pub fn open(credentials: &Credentials, env: &Env) -> Result<Self> {
        let missing = DESCRIPTOR.missing_fields(credentials);
        if !missing.is_empty() {
            return Err(BrokerError::new(
                ErrorKind::Invalid,
                "Enter the API key name and the private key.",
            ));
        }
        let name = credentials.get(FIELD_NAME).unwrap_or_default();
        let secret = credentials.get(FIELD_PRIVATE_KEY).unwrap_or_default();
        let signer = Arc::new(JwtSigner::new(name, secret, Arc::clone(&env.clock))?);
        let scrubber = Scrubber::new(credentials.secret_values());
        let base = env.base_url().unwrap_or(BASE_URL);
        let http = HttpClient::new(
            base,
            Arc::clone(&signer) as Arc<dyn RequestSigner>,
            scrubber,
            env.request_gap(DESCRIPTOR.min_request_gap_ms),
        );

        let permissions: KeyPermissions = decode(
            http.get(&path("/key_permissions"), &[])?,
            "the key's permissions",
        )?;
        if !permissions.can_view {
            return Err(BrokerError::new(
                ErrorKind::Permission,
                "This API key cannot view account data. Create a key with the View permission.",
            ));
        }
        let label = match permissions.portfolio_type.as_str() {
            "DEFAULT" | "CONSUMER" => "Default portfolio".to_owned(),
            "INTX" => "International portfolio".to_owned(),
            "" => "Portfolio".to_owned(),
            other => format!("{} portfolio", title_case(other)),
        };
        let id = if permissions.portfolio_uuid.is_empty() {
            "default".to_owned()
        } else {
            permissions.portfolio_uuid.clone()
        };
        let session = Session {
            permissions: Permissions {
                can_view: permissions.can_view,
                can_trade: permissions.can_trade,
                can_transfer: permissions.can_transfer,
            },
            accounts: vec![AccountRef {
                id: AccountId::new(id),
                label,
                kind: AccountKind::Spot,
            }],
        };
        Ok(Self {
            http,
            session,
            clock: Arc::clone(&env.clock),
            signer,
            ws_url: env.ws_url().unwrap_or(USER_HOST).to_owned(),
            timing: env.stream_timing,
        })
    }

    fn account_id(&self) -> AccountId {
        self.session
            .accounts
            .first()
            .map(|a| a.id.clone())
            .unwrap_or_default()
    }

    fn spec_from(&self, dto: &ProductDto) -> Option<ProductSpec> {
        if dto.product_id.is_empty() {
            return None;
        }
        // Spot only: derivatives have their own models and are not tradable here.
        if !dto.product_type.is_empty() && dto.product_type != "SPOT" {
            return None;
        }
        let increment = |text: &str| parse_decimal(text).filter(|v| *v > Decimal::ZERO);
        let price_increment = increment(&dto.price_increment)?;
        let base_increment = increment(&dto.base_increment)?;
        let quote_increment = increment(&dto.quote_increment).unwrap_or(price_increment);
        Some(ProductSpec {
            id: ProductId::new(&dto.product_id),
            base: dto.base_currency_id.clone(),
            quote: dto.quote_currency_id.clone(),
            display_name: if dto.display_name.is_empty() {
                dto.product_id.clone()
            } else {
                dto.display_name.clone()
            },
            price_increment,
            base_increment,
            quote_increment,
            base_min: parse_decimal(&dto.base_min_size).unwrap_or(base_increment),
            base_max: parse_decimal(&dto.base_max_size),
            quote_min: parse_decimal(&dto.quote_min_size).unwrap_or(Decimal::ZERO),
            quote_max: parse_decimal(&dto.quote_max_size),
            restrictions: Restrictions {
                trading_disabled: dto.trading_disabled || dto.is_disabled,
                cancel_only: dto.cancel_only,
                limit_only: dto.limit_only,
                post_only: dto.post_only,
                auction_mode: dto.auction_mode,
                view_only: dto.view_only,
                offline: !dto.status.is_empty() && !dto.status.eq_ignore_ascii_case("online"),
            },
            fetched_ms: self.clock.now_ms(),
        })
    }
}

fn title_case(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
    })
}

impl Connection for CoinbaseConnection {
    fn broker(&self) -> BrokerId {
        ID
    }

    fn session(&self) -> &Session {
        &self.session
    }

    fn balances(&self) -> Result<Vec<Balance>> {
        let portfolio = self.account_id();
        let mut balances = Vec::new();
        let mut cursor = String::new();
        for _ in 0..MAX_PAGES {
            let limit = ACCOUNT_PAGE.to_string();
            let mut query = vec![("limit", limit.as_str())];
            if !cursor.is_empty() {
                query.push(("cursor", cursor.as_str()));
            }
            let page: AccountsPage = decode(
                self.http.get(&path("/accounts"), &query)?,
                "the account list",
            )?;
            for account in page.accounts {
                let (Some(available), Some(held)) = (
                    parse_decimal(&account.available_balance.value),
                    parse_decimal(&account.hold.value).or(Some(Decimal::ZERO)),
                ) else {
                    continue;
                };
                balances.push(Balance {
                    account: portfolio.clone(),
                    asset: if account.currency.is_empty() {
                        account.available_balance.currency
                    } else {
                        account.currency
                    },
                    available,
                    held,
                });
            }
            if !page.has_next || page.cursor.is_empty() {
                return Ok(balances);
            }
            cursor = page.cursor;
        }
        Err(unreadable("an account list that never ends"))
    }

    fn products(&self) -> Result<Vec<ProductSpec>> {
        let mut specs = Vec::new();
        let mut offset = 0_usize;
        for _ in 0..MAX_PAGES {
            let limit = PRODUCT_PAGE.to_string();
            let offset_text = offset.to_string();
            let page: ProductsPage = decode(
                self.http.get(
                    &path("/products"),
                    &[
                        ("product_type", "SPOT"),
                        ("get_tradability_status", "true"),
                        ("limit", limit.as_str()),
                        ("offset", offset_text.as_str()),
                    ],
                )?,
                "the market list",
            )?;
            let received = page.products.len();
            specs.extend(page.products.iter().filter_map(|dto| self.spec_from(dto)));
            if received < PRODUCT_PAGE {
                return Ok(specs);
            }
            offset += received;
        }
        Err(unreadable("a market list that never ends"))
    }

    fn product(&self, id: &ProductId) -> Result<ProductSpec> {
        let value = self.http.get(
            &path(&format!("/products/{}", id.as_str())),
            &[("get_tradability_status", "true")],
        )?;
        let dto: ProductDto = decode(value, "the market")?;
        self.spec_from(&dto).ok_or_else(|| {
            BrokerError::new(
                ErrorKind::Unsupported,
                format!("{id} is not a spot market that can be traded here."),
            )
        })
    }

    fn quote(&self, id: &ProductId) -> Result<Quote> {
        let book: BestBidAsk = decode(
            self.http
                .get(&path("/best_bid_ask"), &[("product_ids", id.as_str())])?,
            "the best prices",
        )?;
        let book = book
            .pricebooks
            .into_iter()
            .find(|b| b.product_id.is_empty() || b.product_id == id.as_str())
            .ok_or_else(|| BrokerError::new(ErrorKind::NotFound, "No prices for that market."))?;
        let top =
            |levels: &[super::wire::Level]| levels.first().and_then(|l| parse_decimal(&l.price));
        Ok(Quote {
            bid: top(&book.bids),
            ask: top(&book.asks),
            last: None,
            time_ms: parse_rfc3339_ms(&book.time).unwrap_or_else(|| self.clock.now_ms()),
        })
    }

    fn preview(&self, draft: &OrderDraft) -> Result<OrderPreview> {
        self.do_preview(draft)
    }

    fn place(&self, draft: &OrderDraft, client_id: &ClientOrderId) -> Result<PlacedOrder> {
        self.do_place(draft, client_id)
    }

    fn cancel(&self, orders: &[OrderId]) -> Result<Vec<CancelResult>> {
        self.do_cancel(orders)
    }

    fn preview_edit(&self, order: &LiveOrder, change: &OrderChange) -> Result<EditPreview> {
        self.do_preview_edit(order, change)
    }

    fn amend(&self, order: &LiveOrder, change: &OrderChange) -> Result<EditOutcome> {
        self.do_amend(order, change)
    }

    fn order(&self, id: &OrderId) -> Result<LiveOrder> {
        self.do_order(id)
    }

    fn open_orders(&self) -> Result<Vec<LiveOrder>> {
        self.do_open_orders()
    }

    fn order_history(
        &self,
        filter: &HistoryFilter,
        cursor: Option<&str>,
    ) -> Result<Page<LiveOrder>> {
        self.do_order_history(filter, cursor)
    }

    fn fills(&self, filter: &FillFilter, cursor: Option<&str>) -> Result<Page<LiveFill>> {
        self.do_fills(filter, cursor)
    }

    fn find_by_client_id(
        &self,
        client_id: &ClientOrderId,
        since_ms: i64,
    ) -> Result<Option<LiveOrder>> {
        self.do_find_by_client_id(client_id, since_ms)
    }

    fn fee_summary(&self) -> Result<FeeSummary> {
        self.do_fee_summary()
    }

    fn open_stream(&self, sink: Arc<dyn EventSink>) -> Result<StreamHandle> {
        Ok(stream::spawn(
            stream::Config {
                url: self.ws_url.clone(),
                signer: Arc::clone(&self.signer),
                clock: Arc::clone(&self.clock),
                timing: self.timing,
            },
            sink,
        ))
    }

    fn capabilities(&self, spec: &ProductSpec) -> Capabilities {
        if !self.session.permissions.can_trade {
            return Capabilities::blocked(
                "This API key can view but not trade. Create a key with the Trade permission.",
            );
        }
        if let Some(reason) = spec.restrictions.blocked_reason() {
            return Capabilities::blocked(reason);
        }
        let limit_style = spec.restrictions.limit_only || spec.restrictions.post_only;
        Capabilities {
            tradable: Tradability::Yes,
            order_types: if limit_style {
                vec![OrderType::Limit]
            } else {
                vec![OrderType::Market, OrderType::Limit, OrderType::StopLimit]
            },
            tif: vec![
                TifKind::GoodTilCancelled,
                TifKind::GoodTilDate,
                TifKind::FillOrKill,
            ],
            stop_limit_tif: vec![TifKind::GoodTilCancelled, TifKind::GoodTilDate],
            post_only: true,
            quote_sized_market: true,
            // Decided per order by an edit preview; this says the broker has the operation.
            native_edit: true,
            atomic_replace: false,
            brackets: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Broker;
    use crate::coinbase::Coinbase;
    use crate::coinbase::fake::{KEY_NAME, credentials, generate_key_pem as key_pem};
    use crate::testing::mock::{MockServer, Reply};
    use base64::Engine as _;

    fn connect(server: &MockServer, pem: &str) -> Result<Box<dyn Connection>> {
        Coinbase.connect(
            &credentials(pem),
            &Env::default().with_loopback_base_url(server.url.clone()),
        )
    }

    const PERMISSIONS: &str = r#"{"can_view":true,"can_trade":true,"can_transfer":false,"portfolio_uuid":"pf-1","portfolio_type":"DEFAULT"}"#;

    fn exchange(request: &crate::testing::mock::RecordedRequest) -> Reply {
        match request.path() {
            "/api/v3/brokerage/key_permissions" => Reply::json(200, PERMISSIONS),
            "/api/v3/brokerage/accounts" if request.query().contains("cursor=c2") => Reply::json(
                200,
                r#"{"accounts":[{"uuid":"a3","currency":"USD","available_balance":{"value":"1250.55","currency":"USD"},"hold":{"value":"100.00","currency":"USD"},"active":true}],"has_next":false,"cursor":""}"#,
            ),
            "/api/v3/brokerage/accounts" => Reply::json(
                200,
                r#"{"accounts":[
                    {"uuid":"a1","currency":"BTC","available_balance":{"value":"0.5","currency":"BTC"},"hold":{"value":"0.1","currency":"BTC"},"active":true},
                    {"uuid":"a2","currency":"ETH","available_balance":{"value":"","currency":"ETH"},"hold":{"value":"","currency":"ETH"},"active":true}
                ],"has_next":true,"cursor":"c2"}"#,
            ),
            "/api/v3/brokerage/products" => Reply::json(
                200,
                r#"{"products":[
                    {"product_id":"BTC-USD","base_currency_id":"BTC","quote_currency_id":"USD","display_name":"BTC-USD","price_increment":"0.01","base_increment":"0.00000001","quote_increment":"0.01","base_min_size":"0.00000001","base_max_size":"3400","quote_min_size":"1","quote_max_size":"150000000","status":"online","product_type":"SPOT"},
                    {"product_id":"LTC-USD","base_currency_id":"LTC","quote_currency_id":"USD","price_increment":"0.01","base_increment":"0.001","quote_increment":"0.01","limit_only":true,"status":"online","product_type":"SPOT"},
                    {"product_id":"OLD-USD","base_currency_id":"OLD","quote_currency_id":"USD","price_increment":"0.01","base_increment":"1","trading_disabled":true,"status":"delisted"},
                    {"product_id":"BTC-PERP","price_increment":"1","base_increment":"1","product_type":"FUTURE"},
                    {"product_id":"BROKEN-USD","price_increment":"","base_increment":"1"}
                ]}"#,
            ),
            "/api/v3/brokerage/products/BTC-USD" => Reply::json(
                200,
                r#"{"product_id":"BTC-USD","base_currency_id":"BTC","quote_currency_id":"USD","price_increment":"0.01","base_increment":"0.00000001","status":"online","product_type":"SPOT","view_only":true}"#,
            ),
            "/api/v3/brokerage/best_bid_ask" => Reply::json(
                200,
                r#"{"pricebooks":[{"product_id":"BTC-USD","bids":[{"price":"89999.50","size":"1"}],"asks":[{"price":"90000.50","size":"2"}],"time":"2026-11-09T12:00:00.250Z"}]}"#,
            ),
            _ => Reply::json(404, r#"{"message":"no such route"}"#),
        }
    }

    #[test]
    fn connecting_reads_the_key_permissions_and_names_the_portfolio() {
        let server = MockServer::start(exchange);
        let connection = connect(&server, &key_pem()).unwrap();
        let session = connection.session();
        assert_eq!(
            session.permissions,
            Permissions {
                can_view: true,
                can_trade: true,
                can_transfer: false
            }
        );
        assert_eq!(session.accounts[0].id.as_str(), "pf-1");
        assert_eq!(session.accounts[0].label, "Default portfolio");
        assert_eq!(connection.broker(), ID);

        // The request was signed for that exact method, host and path, and sent as a bearer token.
        let seen = &server.requests()[0];
        assert_eq!(seen.path(), "/api/v3/brokerage/key_permissions");
        let auth = seen.header("authorization").unwrap();
        assert!(auth.starts_with("Bearer eyJ"));
        let claims = auth.rsplit('.').nth(1).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(claims)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            claims["uri"].as_str().unwrap(),
            format!(
                "GET {}/api/v3/brokerage/key_permissions",
                server.url.trim_start_matches("http://")
            )
        );
    }

    #[test]
    fn a_key_that_cannot_view_is_refused_and_a_view_only_key_cannot_trade() {
        let server = MockServer::start(|_| {
            Reply::json(
                200,
                r#"{"can_view":false,"can_trade":false,"can_transfer":false}"#,
            )
        });
        let error = connect(&server, &key_pem()).err().unwrap();
        assert_eq!(error.kind, ErrorKind::Permission);

        let server = MockServer::start(|_| {
            Reply::json(
                200,
                r#"{"can_view":true,"can_trade":false,"can_transfer":true,"portfolio_uuid":"p"}"#,
            )
        });
        let connection = connect(&server, &key_pem()).unwrap();
        assert!(connection.session().permissions.can_transfer);
        let spec = connection.product(&ProductId::new("BTC-USD")).err();
        // (the product route is absent here; capabilities are what matters)
        assert!(spec.is_some());
        let blocked = connection.capabilities(&sample_spec());
        assert!(!blocked.is_tradable());
        assert!(
            blocked
                .blocked_reason()
                .unwrap()
                .contains("Trade permission")
        );
    }

    fn sample_spec() -> ProductSpec {
        ProductSpec {
            id: ProductId::new("BTC-USD"),
            base: "BTC".to_owned(),
            quote: "USD".to_owned(),
            display_name: "BTC-USD".to_owned(),
            price_increment: Decimal::new(1, 2),
            base_increment: Decimal::new(1, 8),
            quote_increment: Decimal::new(1, 2),
            base_min: Decimal::new(1, 8),
            base_max: None,
            quote_min: Decimal::ONE,
            quote_max: None,
            restrictions: Restrictions::default(),
            fetched_ms: 0,
        }
    }

    #[test]
    fn credentials_problems_are_reported_without_a_network_call() {
        let server = MockServer::start(exchange);
        let env = Env::default().with_loopback_base_url(server.url.clone());
        let error = Coinbase.connect(&Credentials::new(), &env).err().unwrap();
        assert_eq!(error.kind, ErrorKind::Invalid);
        let error = Coinbase
            .connect(&credentials("not a key"), &env)
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::Auth);
        assert!(!error.to_string().contains("not a key"));
        assert_eq!(server.request_count(), 0);
    }

    #[test]
    fn a_rejected_key_is_an_auth_error_that_does_not_echo_the_key() {
        let pem = key_pem();
        let reply = format!(
            r#"{{"message":"invalid token {} {}","code":"UNAUTHENTICATED"}}"#,
            KEY_NAME,
            pem.lines().nth(1).unwrap()
        );
        let server = MockServer::start(move |_| Reply::json(401, reply.clone()));
        let error = connect(&server, &pem).err().unwrap();
        assert_eq!(error.kind, ErrorKind::Auth);
        let shown = format!("{error} {} {:?}", error.detail, error.code);
        assert!(!shown.contains(KEY_NAME), "{shown}");
        assert!(!shown.contains(pem.lines().nth(1).unwrap()), "{shown}");
    }

    #[test]
    fn balances_follow_the_cursor_and_keep_available_and_held_apart() {
        let server = MockServer::start(exchange);
        let balances = connect(&server, &key_pem()).unwrap().balances().unwrap();
        let by_asset = |asset: &str| balances.iter().find(|b| b.asset == asset);
        assert_eq!(by_asset("BTC").unwrap().available.to_string(), "0.5");
        assert_eq!(by_asset("BTC").unwrap().held.to_string(), "0.1");
        assert_eq!(by_asset("BTC").unwrap().total().to_string(), "0.6");
        assert_eq!(by_asset("USD").unwrap().available.to_string(), "1250.55");
        assert_eq!(by_asset("USD").unwrap().account.as_str(), "pf-1");
        // An account with unreadable amounts is skipped, not shown as zero.
        assert!(by_asset("ETH").is_none());
        let accounts: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.path().ends_with("/accounts"))
            .collect();
        assert_eq!(accounts.len(), 2);
        assert!(accounts[1].query().contains("cursor=c2"));
    }

    #[test]
    fn products_keep_spot_markets_with_readable_rules_and_flag_restrictions() {
        let server = MockServer::start(exchange);
        let connection = connect(&server, &key_pem()).unwrap();
        let products = connection.products().unwrap();
        let ids: Vec<_> = products.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            ["BTC-USD", "LTC-USD", "OLD-USD"],
            "futures and unreadable rows are dropped"
        );
        let btc = &products[0];
        assert_eq!(btc.price_increment.to_string(), "0.01");
        assert_eq!(btc.base_increment.to_string(), "0.00000001");
        assert_eq!(btc.quote_min.to_string(), "1");
        assert_eq!(btc.base_max.unwrap().to_string(), "3400");
        assert!(products[1].restrictions.limit_only);
        assert!(products[2].restrictions.trading_disabled && products[2].restrictions.offline);

        let caps = |spec: &ProductSpec| connection.capabilities(spec);
        assert!(caps(btc).allows(OrderType::StopLimit));
        assert!(caps(btc).quote_sized_market);
        assert_eq!(caps(&products[1]).order_types, vec![OrderType::Limit]);
        assert!(!caps(&products[2]).is_tradable());
        let query = server
            .requests()
            .into_iter()
            .find(|r| r.path().ends_with("/products"))
            .unwrap()
            .query()
            .to_owned();
        assert!(
            query.contains("product_type=SPOT") && query.contains("get_tradability_status=true")
        );
    }

    #[test]
    fn a_view_only_market_is_not_tradable() {
        let server = MockServer::start(exchange);
        let connection = connect(&server, &key_pem()).unwrap();
        let spec = connection.product(&ProductId::new("BTC-USD")).unwrap();
        assert!(spec.restrictions.view_only);
        assert!(!connection.capabilities(&spec).is_tradable());
    }

    #[test]
    fn the_best_prices_come_from_the_top_of_the_book() {
        let server = MockServer::start(exchange);
        let quote = connect(&server, &key_pem())
            .unwrap()
            .quote(&ProductId::new("BTC-USD"))
            .unwrap();
        assert_eq!(quote.bid.unwrap().to_string(), "89999.50");
        assert_eq!(quote.ask.unwrap().to_string(), "90000.50");
        assert_eq!(quote.time_ms, 1_794_225_600_250);
        assert!(
            server
                .requests()
                .iter()
                .any(|r| r.query() == "product_ids=BTC-USD")
        );
    }
}
