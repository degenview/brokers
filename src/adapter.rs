//! What an exchange implements. A [`Broker`] is a stateless description plus a way to connect; a
//! [`Connection`] is one authenticated session. Both are blocking: they run on worker threads and
//! never on an interface thread.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::Result;
use crate::model::capabilities::Capabilities;
use crate::model::orders::{
    CancelResult, EditOutcome, EditPreview, FeeSummary, FillFilter, HistoryFilter, OrderChange,
    OrderPreview, Page, PlacedOrder,
};
use crate::model::types::{
    AccountRef, Balance, BrokerId, ClientOrderId, LiveFill, LiveOrder, OrderDraft, OrderId,
    Permissions, ProductId, ProductSpec, Quote,
};
use crate::secret::Credentials;

/// How a credential field is entered, so a form can render it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldKind {
    /// A short single-line value.
    Text,
    /// A multi-line PEM block.
    Pem,
    /// A base64 string.
    Base64,
}

/// One input of a broker's connect form.
#[derive(Clone, Copy, Debug)]
pub struct CredentialField {
    /// The key in [`Credentials`].
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    /// Hidden by default and never echoed.
    pub secret: bool,
    pub kind: FieldKind,
}

/// How a broker groups what a credential can see.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountModel {
    /// One account per credential.
    Flat,
    /// Portfolios under one login; a credential is scoped to one of them.
    Portfolios,
    /// Sub-accounts the user chooses between.
    SubAccounts,
}

/// The rules for the id we choose for an order before sending it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientIdRules {
    pub max_len: usize,
    /// The broker returns the existing order when an id is reused, instead of creating a second.
    /// When `false`, a retry must look the order up first.
    pub idempotent: bool,
}

/// Static facts about a broker. The interface is drawn from these; nothing else in the application
/// needs to know a broker's name.
#[derive(Clone, Copy, Debug)]
pub struct Descriptor {
    pub id: BrokerId,
    pub display_name: &'static str,
    pub credential_fields: &'static [CredentialField],
    /// How to create a credential, one step per entry.
    pub setup_steps: &'static [&'static str],
    /// What to tick (and not tick) when creating it.
    pub permission_note: &'static str,
    pub docs_url: &'static str,
    pub client_id: ClientIdRules,
    /// The market-data source whose symbols this broker can trade (the application's own slug for
    /// it, such as `coinbase`). A chart from any other source is not tradable here.
    pub market_source: &'static str,
    pub account_model: AccountModel,
    /// The smallest gap between two requests to this broker's private endpoints.
    pub min_request_gap_ms: u64,
}

impl Descriptor {
    /// The fields that are missing or blank, by key.
    pub fn missing_fields(&self, credentials: &Credentials) -> Vec<&'static str> {
        self.credential_fields
            .iter()
            .filter(|field| {
                credentials
                    .get(field.key)
                    .is_none_or(|value| value.trim().is_empty())
            })
            .map(|field| field.key)
            .collect()
    }
}

/// A market as the application names it: the data source's slug and the symbol it uses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MarketRef {
    pub source: String,
    pub symbol: String,
}

/// Which broker market a chart's market is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProductResolution {
    /// This broker's id for the same market.
    Market(ProductId),
    /// The chart's data comes from a source this broker does not trade. Nothing is substituted.
    OtherSource { expected: &'static str },
    /// The symbol cannot be read as a market of this broker.
    Unrecognised,
}

/// What a connection learned about the credential when it was opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    pub permissions: Permissions,
    pub accounts: Vec<AccountRef>,
}

/// A source of the current time in milliseconds since the Unix epoch, so wake-gap and staleness
/// logic can be tested without sleeping.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
            })
    }
}

/// The environment a connection runs in.
#[derive(Clone)]
pub struct Env {
    pub clock: Arc<dyn Clock>,
    base_url: Option<String>,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            clock: Arc::new(SystemClock),
            base_url: None,
        }
    }
}

impl Env {
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Points the adapter at a local mock exchange. Present only in debug and test builds, and
    /// even there only a loopback address is honoured (see [`Env::base_url`]): a signed token must
    /// never be sendable to another host because of a setting.
    #[cfg(any(test, feature = "testing", debug_assertions))]
    #[must_use]
    pub fn with_loopback_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// The override, if there is one and it names a loopback host.
    pub fn base_url(&self) -> Option<&str> {
        self.base_url
            .as_deref()
            .filter(|url| crate::http::is_loopback_url(url))
    }
}

/// One exchange.
pub trait Broker: Send + Sync {
    fn descriptor(&self) -> &'static Descriptor;

    /// Maps a chart's market to this broker's product, without any network call.
    fn resolve_market(&self, market: &MarketRef) -> ProductResolution;

    /// Checks the credentials and opens a session. Reads only: nothing is placed or changed.
    fn connect(&self, credentials: &Credentials, env: &Env) -> Result<Box<dyn Connection>>;
}

/// One authenticated session with a broker.
pub trait Connection: Send + Sync {
    fn broker(&self) -> BrokerId;

    fn session(&self) -> &Session;

    /// Every holding in the credential's account(s), available and held separately.
    fn balances(&self) -> Result<Vec<Balance>>;

    /// Every market the account may see, with its rules.
    fn products(&self) -> Result<Vec<ProductSpec>>;

    /// One market's rules, fresh from the broker.
    fn product(&self, id: &ProductId) -> Result<ProductSpec>;

    /// The best prices the broker would trade at now.
    fn quote(&self, id: &ProductId) -> Result<Quote>;

    /// What can be done in this market with this credential. Pure: computed from the spec and the
    /// session's permissions.
    fn capabilities(&self, spec: &ProductSpec) -> Capabilities;

    /// What the broker says `draft` would do. Places nothing.
    fn preview(&self, draft: &OrderDraft) -> Result<OrderPreview>;

    /// Places an order under `client_id`. **Sent once, never retried.** A failure to complete the
    /// exchange is `ErrorKind::OutcomeUnknown`; resolve it with [`Connection::find_by_client_id`]
    /// (see `submit`), never by sending again blindly.
    fn place(&self, draft: &OrderDraft, client_id: &ClientOrderId) -> Result<PlacedOrder>;

    /// Asks the broker to cancel orders. A `Requested` result is not a cancellation: the order
    /// stays live until its own status says otherwise.
    fn cancel(&self, orders: &[OrderId]) -> Result<Vec<CancelResult>>;

    /// What an edit would do. Places and changes nothing. An error of kind `Unsupported` says the
    /// order cannot be edited, with the reason.
    fn preview_edit(&self, order: &LiveOrder, change: &OrderChange) -> Result<EditPreview>;

    /// Changes an open order in place (or replaces it atomically). Never cancel-and-place.
    fn amend(&self, order: &LiveOrder, change: &OrderChange) -> Result<EditOutcome>;

    /// One order, fresh from the broker.
    fn order(&self, id: &OrderId) -> Result<LiveOrder>;

    /// Every order that may still trade.
    fn open_orders(&self) -> Result<Vec<LiveOrder>>;

    fn order_history(
        &self,
        filter: &HistoryFilter,
        cursor: Option<&str>,
    ) -> Result<Page<LiveOrder>>;

    fn fills(&self, filter: &FillFilter, cursor: Option<&str>) -> Result<Page<LiveFill>>;

    /// The order placed under `client_id` at or after `since_ms`, if the broker lists one.
    fn find_by_client_id(
        &self,
        client_id: &ClientOrderId,
        since_ms: i64,
    ) -> Result<Option<LiveOrder>>;

    fn fee_summary(&self) -> Result<FeeSummary>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIELDS: &[CredentialField] = &[
        CredentialField {
            key: "a",
            label: "A",
            hint: "",
            secret: false,
            kind: FieldKind::Text,
        },
        CredentialField {
            key: "b",
            label: "B",
            hint: "",
            secret: true,
            kind: FieldKind::Pem,
        },
    ];

    fn descriptor() -> Descriptor {
        Descriptor {
            id: BrokerId::new("test"),
            display_name: "Test",
            credential_fields: FIELDS,
            setup_steps: &[],
            permission_note: "",
            docs_url: "",
            client_id: ClientIdRules {
                max_len: 64,
                idempotent: true,
            },
            market_source: "test",
            account_model: AccountModel::Flat,
            min_request_gap_ms: 100,
        }
    }

    #[test]
    fn missing_fields_lists_blank_and_absent_keys() {
        let descriptor = descriptor();
        assert_eq!(
            descriptor.missing_fields(&Credentials::new()),
            vec!["a", "b"]
        );
        let partial = Credentials::new().with("a", "x").with("b", "  ");
        assert_eq!(descriptor.missing_fields(&partial), vec!["b"]);
        let full = Credentials::new().with("a", "x").with("b", "y");
        assert!(descriptor.missing_fields(&full).is_empty());
    }

    #[test]
    fn a_base_url_override_is_honoured_for_loopback_only() {
        let local = Env::default().with_loopback_base_url("http://127.0.0.1:9999");
        assert_eq!(local.base_url(), Some("http://127.0.0.1:9999"));
        let remote = Env::default().with_loopback_base_url("https://evil.example.com");
        assert_eq!(remote.base_url(), None);
        let sneaky = Env::default().with_loopback_base_url("http://127.0.0.1.evil.example.com");
        assert_eq!(sneaky.base_url(), None);
        assert_eq!(Env::default().base_url(), None);
    }
}
