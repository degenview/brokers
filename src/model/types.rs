//! The venue-neutral trading vocabulary. Every price, size and fee is a [`Decimal`]: an exchange
//! sends them as strings and an order must go back out with exactly the digits the user confirmed.
//!
//! Identifiers are the broker's own strings and are never rewritten (`BTC-USD` stays `BTC-USD`).

use std::fmt;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::registry;

/// Which broker. Interned: the only values are the ones a [`crate::Broker`] in the registry
/// declares, so comparing is a pointer-sized copy and a stored id that no longer exists fails to
/// load instead of becoming a broker nobody can talk to.
#[derive(Clone, Copy, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct BrokerId(&'static str);

impl BrokerId {
    /// For a broker's own descriptor. Ids are lower-case ASCII (`acme`).
    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl fmt::Debug for BrokerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl fmt::Display for BrokerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Serialize for BrokerId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for BrokerId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        registry::id_for(&text)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown broker `{text}`")))
    }
}

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Default, Eq, Hash, PartialEq, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id! {
    /// A tradable market in the broker's own spelling (`BTC-USD`).
    ProductId
}
string_id! {
    /// The broker's id for an order.
    OrderId
}
string_id! {
    /// The id we chose for an order before sending it. It is how an order whose submission outcome
    /// is unknown is found again.
    ClientOrderId
}
string_id! {
    /// A portfolio, sub-account or account at the broker.
    AccountId
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

/// A size is stated in the base asset (0.01 BTC) or the quote asset (25 USD). Never both.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OrderSize {
    Base(Decimal),
    Quote(Decimal),
}

impl OrderSize {
    pub fn amount(self) -> Decimal {
        match self {
            Self::Base(value) | Self::Quote(value) => value,
        }
    }
}

/// How long an order stays working.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TimeInForce {
    GoodTilCancelled,
    /// Until this instant (milliseconds since the Unix epoch).
    GoodTilDate(i64),
    ImmediateOrCancel,
    FillOrKill,
}

impl TimeInForce {
    pub fn kind(self) -> TifKind {
        match self {
            Self::GoodTilCancelled => TifKind::GoodTilCancelled,
            Self::GoodTilDate(_) => TifKind::GoodTilDate,
            Self::ImmediateOrCancel => TifKind::ImmediateOrCancel,
            Self::FillOrKill => TifKind::FillOrKill,
        }
    }
}

/// [`TimeInForce`] without its payload, for capability lists.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum TifKind {
    GoodTilCancelled,
    GoodTilDate,
    ImmediateOrCancel,
    FillOrKill,
}

/// The kinds of order a user can ask for.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit,
    /// A limit order that rests until the stop price trades. Spot venues rarely have a plain
    /// stop-market, so "stop" always means this.
    StopLimit,
    /// A kind this crate can show and cancel but not create or edit (a bracket or TWAP placed in
    /// the broker's own app, say).
    Other,
}

/// Which way price must move through the stop to trigger it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum StopDirection {
    /// Triggers when price rises to or through the stop.
    Up,
    /// Triggers when price falls to or through the stop.
    Down,
}

impl StopDirection {
    /// The direction a stop-limit has when it protects against a move the wrong way: a sell stop
    /// below the market triggers downward, a buy stop above it upward. Derived from the draft
    /// rather than asked for, but shown to the user.
    pub fn for_stop(side: Side, stop: Decimal, reference: Decimal) -> Self {
        match side {
            Side::Sell if stop <= reference => Self::Down,
            Side::Sell => Self::Up,
            Side::Buy if stop >= reference => Self::Up,
            Side::Buy => Self::Down,
        }
    }
}

/// What a user is asking the broker to do. Built by a ticket, checked by
/// [`crate::validate::validate_draft`], translated by an adapter.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrderDraft {
    pub product: ProductId,
    pub side: Side,
    pub kind: OrderKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OrderKind {
    /// Executes at the best available price now.
    Market { size: OrderSize },
    Limit {
        base_size: Decimal,
        price: Decimal,
        tif: TimeInForce,
        post_only: bool,
    },
    StopLimit {
        base_size: Decimal,
        stop_price: Decimal,
        limit_price: Decimal,
        direction: StopDirection,
        tif: TimeInForce,
    },
}

impl OrderDraft {
    pub fn order_type(&self) -> OrderType {
        match self.kind {
            OrderKind::Market { .. } => OrderType::Market,
            OrderKind::Limit { .. } => OrderType::Limit,
            OrderKind::StopLimit { .. } => OrderType::StopLimit,
        }
    }

    /// The price that decides notional for a limit-style order; `None` for a market order.
    pub fn working_price(&self) -> Option<Decimal> {
        match self.kind {
            OrderKind::Market { .. } => None,
            OrderKind::Limit { price, .. } => Some(price),
            OrderKind::StopLimit { limit_price, .. } => Some(limit_price),
        }
    }

    pub fn base_size(&self) -> Option<Decimal> {
        match self.kind {
            OrderKind::Market {
                size: OrderSize::Base(size),
            }
            | OrderKind::Limit {
                base_size: size, ..
            }
            | OrderKind::StopLimit {
                base_size: size, ..
            } => Some(size),
            OrderKind::Market {
                size: OrderSize::Quote(_),
            } => None,
        }
    }
}

/// The broker's order lifecycle, as the broker reports it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum OrderStatus {
    /// Accepted but not yet working.
    Pending,
    Open,
    /// A cancel was accepted and is being processed. The order can still fill.
    CancelQueued,
    /// An edit was accepted and is being processed. The order can still fill at its old terms.
    EditQueued,
    Filled,
    Cancelled,
    Expired,
    Failed,
    /// The broker sent a status this crate does not know. Treated as non-terminal.
    Unknown,
}

impl OrderStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Cancelled | Self::Expired | Self::Failed
        )
    }

    /// Whether the order may still trade.
    pub fn is_working(self) -> bool {
        matches!(
            self,
            Self::Pending | Self::Open | Self::CancelQueued | Self::EditQueued | Self::Unknown
        )
    }

    /// How far along the lifecycle a status is, so an old update never undoes a newer one.
    /// Terminal states outrank everything.
    pub fn rank(self) -> u8 {
        match self {
            Self::Unknown => 0,
            Self::Pending => 1,
            Self::Open => 2,
            Self::EditQueued => 3,
            Self::CancelQueued => 4,
            Self::Filled | Self::Cancelled | Self::Expired | Self::Failed => 10,
        }
    }
}

/// An order as the broker reports it. Authoritative: nothing local edits one of these.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LiveOrder {
    pub id: OrderId,
    pub client_id: Option<ClientOrderId>,
    pub product: ProductId,
    pub side: Side,
    pub order_type: OrderType,
    pub status: OrderStatus,
    pub tif: Option<TimeInForce>,
    /// The size as it was asked for, in the unit it was asked in.
    pub size: Option<OrderSize>,
    pub limit_price: Option<Decimal>,
    pub stop_price: Option<Decimal>,
    pub stop_direction: Option<StopDirection>,
    pub post_only: bool,
    /// Base amount filled so far.
    pub filled_size: Decimal,
    /// Quote value filled so far.
    pub filled_value: Decimal,
    pub average_price: Option<Decimal>,
    pub fees: Decimal,
    pub created_ms: Option<i64>,
    pub updated_ms: Option<i64>,
    /// The broker's reason when it refused or cancelled the order, in its words.
    pub reason: Option<String>,
    /// Whether the stop of a stop-limit has triggered.
    pub stop_triggered: bool,
}

impl LiveOrder {
    /// Base amount still to fill, when the order was sized in base. A quote-sized order has no
    /// remaining base until it has filled, so it is `None`.
    pub fn remaining_base(&self) -> Option<Decimal> {
        match self.size? {
            OrderSize::Base(size) => Some((size - self.filled_size).max(Decimal::ZERO)),
            OrderSize::Quote(_) => None,
        }
    }

    /// The price an order line sits at: a stop-limit rests at its stop until it triggers.
    pub fn marker_price(&self) -> Option<Decimal> {
        match self.order_type {
            OrderType::StopLimit if !self.stop_triggered => self.stop_price.or(self.limit_price),
            _ => self.limit_price.or(self.stop_price),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Liquidity {
    Maker,
    Taker,
}

/// One execution.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LiveFill {
    /// The broker's id for this execution; unique per fill.
    pub id: String,
    pub order_id: OrderId,
    pub product: ProductId,
    pub side: Side,
    pub price: Decimal,
    /// Base amount.
    pub size: Decimal,
    pub fee: Decimal,
    pub liquidity: Option<Liquidity>,
    pub time_ms: i64,
}

/// A holding. `available + held` is the total; held is committed to open orders.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Balance {
    pub account: AccountId,
    /// The asset's ticker as the broker spells it (`BTC`, `USD`).
    pub asset: String,
    pub available: Decimal,
    pub held: Decimal,
}

impl Balance {
    pub fn total(&self) -> Decimal {
        self.available + self.held
    }
}

/// What kind of account this is. Only `Spot` is implemented; margin and derivatives will bring
/// their own position and margin models rather than reuse spot balances.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AccountKind {
    Spot,
    Margin,
    Derivatives,
}

/// One account at a broker: a portfolio, a sub-account, or the account itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccountRef {
    pub id: AccountId,
    pub label: String,
    pub kind: AccountKind,
}

/// What a credential may do, as far as the broker will say.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Permissions {
    pub can_view: bool,
    pub can_trade: bool,
    /// Whether the credential could move funds. DegenView never does, so a `true` here is a
    /// warning to the user, not a feature.
    pub can_transfer: bool,
}

/// Rules a product can carry that limit what may be placed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Restrictions {
    pub trading_disabled: bool,
    pub cancel_only: bool,
    pub limit_only: bool,
    pub post_only: bool,
    pub auction_mode: bool,
    /// Visible but not tradable by this account (region, eligibility).
    pub view_only: bool,
    pub offline: bool,
}

impl Restrictions {
    /// Why nothing can be placed, or `None` when something can.
    pub fn blocked_reason(&self) -> Option<&'static str> {
        if self.offline {
            Some("This market is offline.")
        } else if self.trading_disabled {
            Some("Trading is disabled for this market.")
        } else if self.view_only {
            Some("This account can view this market but not trade it.")
        } else if self.cancel_only {
            Some("This market only accepts cancellations right now.")
        } else if self.auction_mode {
            Some("This market is in an auction; orders are not accepted until it ends.")
        } else {
            None
        }
    }
}

/// Everything needed to validate an order for one market, from the broker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProductSpec {
    pub id: ProductId,
    pub base: String,
    pub quote: String,
    pub display_name: String,
    pub price_increment: Decimal,
    pub base_increment: Decimal,
    pub quote_increment: Decimal,
    pub base_min: Decimal,
    pub base_max: Option<Decimal>,
    pub quote_min: Decimal,
    pub quote_max: Option<Decimal>,
    pub restrictions: Restrictions,
    /// When this was read from the broker (milliseconds since the Unix epoch). A spec older than
    /// [`ProductSpec::STALE_AFTER_MS`] is not trusted for submission.
    pub fetched_ms: i64,
}

impl ProductSpec {
    /// How long a spec stays trustworthy for placing an order.
    pub const STALE_AFTER_MS: i64 = 10 * 60 * 1000;

    pub fn is_stale(&self, now_ms: i64) -> bool {
        now_ms.saturating_sub(self.fetched_ms) > Self::STALE_AFTER_MS
    }
}

/// The best prices the broker would trade at right now.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    pub bid: Option<Decimal>,
    pub ask: Option<Decimal>,
    pub last: Option<Decimal>,
    pub time_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_support::d;

    #[test]
    fn terminal_statuses_outrank_working_ones() {
        assert!(OrderStatus::Filled.rank() > OrderStatus::CancelQueued.rank());
        assert!(OrderStatus::CancelQueued.rank() > OrderStatus::Open.rank());
        assert!(OrderStatus::Open.rank() > OrderStatus::Pending.rank());
        assert!(OrderStatus::Cancelled.is_terminal());
        assert!(!OrderStatus::CancelQueued.is_terminal());
        assert!(OrderStatus::CancelQueued.is_working());
    }

    #[test]
    fn stop_direction_follows_side_and_reference() {
        let reference = d("100");
        assert_eq!(
            StopDirection::for_stop(Side::Sell, d("95"), reference),
            StopDirection::Down
        );
        assert_eq!(
            StopDirection::for_stop(Side::Sell, d("105"), reference),
            StopDirection::Up
        );
        assert_eq!(
            StopDirection::for_stop(Side::Buy, d("105"), reference),
            StopDirection::Up
        );
        assert_eq!(
            StopDirection::for_stop(Side::Buy, d("95"), reference),
            StopDirection::Down
        );
    }

    #[test]
    fn remaining_base_never_goes_negative_and_skips_quote_sized_orders() {
        let mut order = sample_order();
        order.size = Some(OrderSize::Base(d("1.0")));
        order.filled_size = d("0.25");
        assert_eq!(order.remaining_base(), Some(d("0.75")));
        order.filled_size = d("1.5");
        assert_eq!(order.remaining_base(), Some(Decimal::ZERO));
        order.size = Some(OrderSize::Quote(d("50")));
        assert_eq!(order.remaining_base(), None);
    }

    #[test]
    fn a_stop_limit_rests_at_its_stop_until_it_triggers() {
        let mut order = sample_order();
        order.order_type = OrderType::StopLimit;
        order.stop_price = Some(d("90"));
        order.limit_price = Some(d("89"));
        assert_eq!(order.marker_price(), Some(d("90")));
        order.stop_triggered = true;
        assert_eq!(order.marker_price(), Some(d("89")));
    }

    #[test]
    fn restrictions_explain_what_blocks_a_market() {
        assert_eq!(Restrictions::default().blocked_reason(), None);
        let cancel_only = Restrictions {
            cancel_only: true,
            ..Restrictions::default()
        };
        assert!(
            cancel_only
                .blocked_reason()
                .unwrap()
                .contains("cancellations")
        );
        // A market that only accepts limit orders is still tradable.
        let limit_only = Restrictions {
            limit_only: true,
            ..Restrictions::default()
        };
        assert_eq!(limit_only.blocked_reason(), None);
    }

    #[test]
    fn broker_ids_that_are_not_registered_do_not_deserialize() {
        let parsed: Result<BrokerId, _> = serde_json::from_str("\"no-such-broker\"");
        assert!(parsed.is_err());
    }

    fn sample_order() -> LiveOrder {
        LiveOrder {
            id: OrderId::new("o1"),
            client_id: None,
            product: ProductId::new("BTC-USD"),
            side: Side::Buy,
            order_type: OrderType::Limit,
            status: OrderStatus::Open,
            tif: Some(TimeInForce::GoodTilCancelled),
            size: Some(OrderSize::Base(d("1"))),
            limit_price: Some(d("100")),
            stop_price: None,
            stop_direction: None,
            post_only: false,
            filled_size: Decimal::ZERO,
            filled_value: Decimal::ZERO,
            average_price: None,
            fees: Decimal::ZERO,
            created_ms: None,
            updated_ms: None,
            reason: None,
            stop_triggered: false,
        }
    }
}
