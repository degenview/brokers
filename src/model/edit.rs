//! The rules for changing an open order in place: which orders can be, and what is wrong with a
//! proposed change before the broker is asked. Pure. Each reason is a value; the words belong to the
//! application.

use rust_decimal::Decimal;

use super::capabilities::Capabilities;
use super::orders::OrderChange;
use super::types::{LiveOrder, OrderSize, OrderStatus, OrderType, ProductSpec};
use super::validate::{floor_to_increment, is_multiple};

/// Why an open order cannot be edited.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditBlock {
    /// Only open orders can be edited.
    NotOpen,
    /// A cancel is in progress for this order.
    Cancelling,
    /// Only limit and stop-limit orders can be edited.
    TypeNotEditable,
    /// An order sized by an amount to spend has no base size to change.
    SizedByAmount,
    /// The broker cannot change this market's orders in place.
    NotSupported,
    /// The market's rules have not been read yet.
    RulesNotRead,
}

/// Why `order` cannot be edited, or `None` when it can. `caps` are the market's capabilities, if read.
pub fn edit_block(
    order: &LiveOrder,
    caps: Option<&Capabilities>,
    cancelling: bool,
) -> Option<EditBlock> {
    if order.status != OrderStatus::Open {
        return Some(EditBlock::NotOpen);
    }
    if cancelling {
        return Some(EditBlock::Cancelling);
    }
    if !matches!(order.order_type, OrderType::Limit | OrderType::StopLimit) {
        return Some(EditBlock::TypeNotEditable);
    }
    if matches!(order.size, Some(OrderSize::Quote(_))) || order.size.is_none() {
        return Some(EditBlock::SizedByAmount);
    }
    match caps {
        Some(caps) if caps.native_edit => None,
        Some(_) => Some(EditBlock::NotSupported),
        None => Some(EditBlock::RulesNotRead),
    }
}

/// What is wrong with a change, by the market's own rules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditIssue {
    /// The price is not a multiple of the increment. `nearest_below` is a suggestion only.
    PriceOffIncrement {
        increment: Decimal,
        nearest_below: Decimal,
    },
    SizeOffIncrement {
        increment: Decimal,
    },
    SizeBelowMinimum {
        minimum: Decimal,
    },
    /// The size would be less than what has already filled.
    SizeBelowFilled,
}

/// What is wrong with `change` on `order` in a market with these rules, before the broker is asked.
pub fn edit_issues(order: &LiveOrder, change: &OrderChange, spec: &ProductSpec) -> Vec<EditIssue> {
    let mut issues = Vec::new();
    if let Some(price) = change.price.or(change.stop_price)
        && !is_multiple(price, spec.price_increment)
    {
        issues.push(EditIssue::PriceOffIncrement {
            increment: spec.price_increment,
            nearest_below: floor_to_increment(price, spec.price_increment),
        });
    }
    if let Some(size) = change.size {
        if !is_multiple(size, spec.base_increment) {
            issues.push(EditIssue::SizeOffIncrement {
                increment: spec.base_increment,
            });
        }
        if size < spec.base_min {
            issues.push(EditIssue::SizeBelowMinimum {
                minimum: spec.base_min,
            });
        }
        if size < order.filled_size {
            issues.push(EditIssue::SizeBelowFilled);
        }
    }
    issues
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::test_support::d;
    use crate::model::types::{OrderId, ProductId, Restrictions, Side};

    pub fn order() -> LiveOrder {
        LiveOrder {
            id: OrderId::new("o-1"),
            client_id: None,
            product: ProductId::new("BTC-USD"),
            side: Side::Sell,
            order_type: OrderType::Limit,
            status: OrderStatus::Open,
            tif: None,
            size: Some(OrderSize::Base(d("1"))),
            limit_price: Some(d("100")),
            stop_price: None,
            stop_direction: None,
            post_only: false,
            filled_size: d("0"),
            filled_value: d("0"),
            average_price: None,
            fees: d("0"),
            created_ms: Some(0),
            updated_ms: None,
            reason: None,
            stop_triggered: false,
        }
    }

    pub fn spec() -> ProductSpec {
        ProductSpec {
            id: ProductId::new("BTC-USD"),
            base: "BTC".to_owned(),
            quote: "USD".to_owned(),
            display_name: "BTC-USD".to_owned(),
            price_increment: d("0.01"),
            base_increment: d("0.001"),
            quote_increment: d("0.01"),
            base_min: d("0.01"),
            base_max: None,
            quote_min: d("1"),
            quote_max: None,
            restrictions: Restrictions::default(),
            fetched_ms: 0,
        }
    }

    fn editable() -> Capabilities {
        let mut caps = Capabilities::blocked("x");
        caps.tradable = crate::model::capabilities::Tradability::Yes;
        caps.native_edit = true;
        caps
    }

    #[test]
    fn an_open_limit_order_in_a_market_that_edits_in_place_can_be_edited() {
        assert_eq!(edit_block(&order(), Some(&editable()), false), None);
    }

    #[test]
    fn each_reason_an_order_cannot_be_edited_is_told_apart_in_order() {
        let caps = editable();
        let mut o = order();
        o.status = OrderStatus::Filled;
        assert_eq!(edit_block(&o, Some(&caps), true), Some(EditBlock::NotOpen));
        let o = order();
        assert_eq!(
            edit_block(&o, Some(&caps), true),
            Some(EditBlock::Cancelling)
        );
        let mut o = order();
        o.order_type = OrderType::Market;
        assert_eq!(
            edit_block(&o, Some(&caps), false),
            Some(EditBlock::TypeNotEditable)
        );
        let mut o = order();
        o.size = Some(OrderSize::Quote(d("50")));
        assert_eq!(
            edit_block(&o, Some(&caps), false),
            Some(EditBlock::SizedByAmount)
        );
        let mut o = order();
        o.size = None;
        assert_eq!(
            edit_block(&o, Some(&caps), false),
            Some(EditBlock::SizedByAmount)
        );
        let mut no_edit = caps.clone();
        no_edit.native_edit = false;
        assert_eq!(
            edit_block(&order(), Some(&no_edit), false),
            Some(EditBlock::NotSupported)
        );
        assert_eq!(
            edit_block(&order(), None, false),
            Some(EditBlock::RulesNotRead)
        );
    }

    #[test]
    fn a_price_and_a_size_are_checked_against_the_markets_increments_and_minimum() {
        let off_price = OrderChange {
            price: Some(d("100.005")),
            ..OrderChange::default()
        };
        assert_eq!(
            edit_issues(&order(), &off_price, &spec()),
            [EditIssue::PriceOffIncrement {
                increment: d("0.01"),
                nearest_below: d("100.00")
            }]
        );
        // A stop price is a price too.
        let off_stop = OrderChange {
            stop_price: Some(d("95.123")),
            ..OrderChange::default()
        };
        assert_eq!(edit_issues(&order(), &off_stop, &spec()).len(), 1);
        let small = OrderChange {
            size: Some(d("0.0005")),
            ..OrderChange::default()
        };
        let issues = edit_issues(&order(), &small, &spec());
        assert!(issues.contains(&EditIssue::SizeOffIncrement {
            increment: d("0.001")
        }));
        assert!(issues.contains(&EditIssue::SizeBelowMinimum { minimum: d("0.01") }));
        let fine = OrderChange {
            price: Some(d("101")),
            size: Some(d("0.5")),
            ..OrderChange::default()
        };
        assert!(edit_issues(&order(), &fine, &spec()).is_empty());
    }

    #[test]
    fn the_size_cannot_go_below_what_has_filled() {
        let mut partly = order();
        partly.filled_size = d("0.6");
        let change = OrderChange {
            size: Some(d("0.5")),
            ..OrderChange::default()
        };
        assert_eq!(
            edit_issues(&partly, &change, &spec()),
            [EditIssue::SizeBelowFilled]
        );
    }
}
