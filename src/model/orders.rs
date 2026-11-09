//! The request and reply shapes of order operations, venue-neutral.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::types::{OrderId, OrderStatus, ProductId, Side};

/// A problem the broker found with an order or an edit before it was placed. Shown inline.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Problem {
    /// The broker's code (`PREVIEW_INSUFFICIENT_FUND`), when it sent one.
    pub code: Option<String>,
    pub text: String,
}

/// What the broker says an order would do, without placing it.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrderPreview {
    /// Total in the quote asset, fees included, as the broker computes it.
    pub order_total: Option<Decimal>,
    pub fees: Option<Decimal>,
    pub base_size: Option<Decimal>,
    pub quote_size: Option<Decimal>,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub estimated_average_price: Option<Decimal>,
    /// Estimated slippage as a fraction (0.001 is 0.1%).
    pub slippage: Option<Decimal>,
    /// The broker will refuse the order for these reasons.
    pub problems: Vec<Problem>,
    /// The broker will accept it but wants the user to know.
    pub warnings: Vec<String>,
}

impl OrderPreview {
    pub fn is_acceptable(&self) -> bool {
        self.problems.is_empty()
    }
}

/// An order the broker accepted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlacedOrder {
    pub order_id: OrderId,
    pub product: ProductId,
    pub side: Side,
}

/// What became of one cancel request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CancelOutcome {
    /// The cancel was accepted for processing. The order can still fill until it is cancelled;
    /// only the order's own status says it is.
    Requested,
    /// It filled before the cancel arrived.
    AlreadyFilled,
    /// The broker does not know the order (any more).
    NotFound,
    /// A replace is in progress; try again when it settles.
    BeingReplaced,
    /// The broker refused, with this sentence.
    Rejected(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CancelResult {
    pub order_id: OrderId,
    pub outcome: CancelOutcome,
}

/// A change to an open order. `None` leaves the field as it is.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrderChange {
    /// The new limit price.
    pub price: Option<Decimal>,
    /// The new total size in the base asset.
    pub size: Option<Decimal>,
    /// The new stop price of a stop-limit order.
    pub stop_price: Option<Decimal>,
}

impl OrderChange {
    pub fn is_empty(&self) -> bool {
        self.price.is_none() && self.size.is_none() && self.stop_price.is_none()
    }
}

/// What an edit would do.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EditPreview {
    pub order_total: Option<Decimal>,
    pub fees: Option<Decimal>,
    pub base_size: Option<Decimal>,
    pub quote_size: Option<Decimal>,
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub problems: Vec<Problem>,
}

impl EditPreview {
    pub fn is_acceptable(&self) -> bool {
        self.problems.is_empty()
    }
}

/// How the broker applied an edit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EditOutcome {
    /// The order was changed in place. It keeps its id; the broker's order status moves through
    /// `EditQueued` and the new terms appear when it settles.
    Native,
    /// The broker replaced the order atomically under a new id.
    ReplacedAtomically(OrderId),
}

/// One page of a list, with where the next one starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryFilter {
    /// Empty means every status.
    pub statuses: Vec<OrderStatus>,
    pub product: Option<ProductId>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    /// Page size; the adapter clamps it to what the broker allows.
    pub limit: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FillFilter {
    pub product: Option<ProductId>,
    pub order: Option<OrderId>,
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub limit: u32,
}

/// The account's fee schedule.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FeeSummary {
    pub tier: String,
    /// As fractions (0.006 is 0.6%).
    pub maker_rate: Option<Decimal>,
    pub taker_rate: Option<Decimal>,
}
