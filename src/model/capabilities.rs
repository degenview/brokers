//! What can be done in one market with one credential. An adapter computes this from the broker's
//! rules and the product's flags; the interface shows only what it allows and says why otherwise.

use serde::{Deserialize, Serialize};

use super::types::{OrderType, TifKind};

/// Whether anything can be placed at all.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Tradability {
    Yes,
    /// Nothing can be placed, and this is the sentence that says why.
    No(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    pub tradable: Tradability,
    pub order_types: Vec<OrderType>,
    /// Time-in-force choices for limit orders.
    pub tif: Vec<TifKind>,
    /// Time-in-force choices for stop-limit orders (often narrower than for limits).
    pub stop_limit_tif: Vec<TifKind>,
    pub post_only: bool,
    /// A market buy may be sized in the quote asset ("spend 25 USD").
    pub quote_sized_market: bool,
    /// An open order's price or size can be changed in place.
    pub native_edit: bool,
    /// An edit is performed as one atomic replace (the order gets a new id but cannot be lost).
    pub atomic_replace: bool,
    pub brackets: bool,
}

impl Capabilities {
    /// Nothing is possible; `reason` is shown to the user.
    pub fn blocked(reason: impl Into<String>) -> Self {
        Self {
            tradable: Tradability::No(reason.into()),
            order_types: Vec::new(),
            tif: Vec::new(),
            stop_limit_tif: Vec::new(),
            post_only: false,
            quote_sized_market: false,
            native_edit: false,
            atomic_replace: false,
            brackets: false,
        }
    }

    pub fn is_tradable(&self) -> bool {
        matches!(self.tradable, Tradability::Yes)
    }

    /// The reason nothing can be placed, if that is so.
    pub fn blocked_reason(&self) -> Option<&str> {
        match &self.tradable {
            Tradability::Yes => None,
            Tradability::No(reason) => Some(reason),
        }
    }

    pub fn allows(&self, order_type: OrderType) -> bool {
        self.is_tradable() && self.order_types.contains(&order_type)
    }

    pub fn allows_tif(&self, tif: TifKind) -> bool {
        self.tif.contains(&tif)
    }

    pub fn allows_stop_limit_tif(&self, tif: TifKind) -> bool {
        self.stop_limit_tif.contains(&tif)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_market_allows_nothing_and_says_why() {
        let blocked = Capabilities::blocked("Trading is disabled.");
        assert!(!blocked.is_tradable());
        assert!(!blocked.allows(OrderType::Limit));
        assert_eq!(blocked.blocked_reason(), Some("Trading is disabled."));
    }

    #[test]
    fn a_listed_type_is_allowed_only_while_tradable() {
        let mut caps = Capabilities::blocked("x");
        caps.order_types = vec![OrderType::Limit];
        assert!(!caps.allows(OrderType::Limit));
        caps.tradable = Tradability::Yes;
        assert!(caps.allows(OrderType::Limit));
        assert!(!caps.allows(OrderType::Market));
    }
}
