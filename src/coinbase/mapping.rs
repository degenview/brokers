//! Between the neutral model and Coinbase's order JSON. Pure: no network, no clock.

use rust_decimal::Decimal;
use serde_json::{Value, json};

use super::time::{format_rfc3339_ms, parse_rfc3339_ms};
use super::wire::{FillDto, OrderDto, parse_decimal};
use crate::error::{BrokerError, ErrorKind, Result};
use crate::model::types::{
    ClientOrderId, Liquidity, LiveFill, LiveOrder, OrderDraft, OrderId, OrderKind, OrderSize,
    OrderStatus, OrderType, ProductId, Side, StopDirection, TimeInForce,
};

fn side_text(side: Side) -> &'static str {
    match side {
        Side::Buy => "BUY",
        Side::Sell => "SELL",
    }
}

pub fn side_from(text: &str) -> Option<Side> {
    match text {
        "BUY" => Some(Side::Buy),
        "SELL" => Some(Side::Sell),
        _ => None,
    }
}

/// `order_configuration` for `draft`. Only configurations this crate has verified against the
/// published specification are produced; anything else is `Unsupported`, never approximated.
pub fn order_configuration(draft: &OrderDraft) -> Result<Value> {
    let unsupported = |what: &str| {
        BrokerError::unsupported(format!("{what} is not available for Coinbase orders."))
    };
    Ok(match &draft.kind {
        OrderKind::Market { size } => match size {
            OrderSize::Base(size) => json!({"market_market_ioc": {"base_size": size.to_string()}}),
            OrderSize::Quote(size) => {
                json!({"market_market_ioc": {"quote_size": size.to_string()}})
            }
        },
        OrderKind::Limit {
            base_size,
            price,
            tif,
            post_only,
        } => match tif {
            TimeInForce::GoodTilCancelled => json!({"limit_limit_gtc": {
                "base_size": base_size.to_string(),
                "limit_price": price.to_string(),
                "post_only": post_only,
            }}),
            TimeInForce::GoodTilDate(end) => json!({"limit_limit_gtd": {
                "base_size": base_size.to_string(),
                "limit_price": price.to_string(),
                "end_time": format_rfc3339_ms(*end),
                "post_only": post_only,
            }}),
            TimeInForce::FillOrKill => {
                if *post_only {
                    return Err(BrokerError::invalid(
                        "A fill-or-kill order cannot be post-only.",
                    ));
                }
                json!({"limit_limit_fok": {
                    "base_size": base_size.to_string(),
                    "limit_price": price.to_string(),
                }})
            }
            TimeInForce::ImmediateOrCancel => return Err(unsupported("Immediate-or-cancel")),
        },
        OrderKind::StopLimit {
            base_size,
            stop_price,
            limit_price,
            direction,
            tif,
        } => {
            let direction = match direction {
                StopDirection::Up => "STOP_DIRECTION_STOP_UP",
                StopDirection::Down => "STOP_DIRECTION_STOP_DOWN",
            };
            match tif {
                TimeInForce::GoodTilCancelled => json!({"stop_limit_stop_limit_gtc": {
                    "base_size": base_size.to_string(),
                    "limit_price": limit_price.to_string(),
                    "stop_price": stop_price.to_string(),
                    "stop_direction": direction,
                }}),
                TimeInForce::GoodTilDate(end) => json!({"stop_limit_stop_limit_gtd": {
                    "base_size": base_size.to_string(),
                    "limit_price": limit_price.to_string(),
                    "stop_price": stop_price.to_string(),
                    "end_time": format_rfc3339_ms(*end),
                    "stop_direction": direction,
                }}),
                _ => return Err(unsupported("That time in force on a stop-limit order")),
            }
        }
    })
}

/// The body shared by create and preview (the create adds the client order id).
pub fn order_body(draft: &OrderDraft) -> Result<Value> {
    Ok(json!({
        "product_id": draft.product.as_str(),
        "side": side_text(draft.side),
        "order_configuration": order_configuration(draft)?,
    }))
}

pub fn create_body(draft: &OrderDraft, client_id: &ClientOrderId) -> Result<Value> {
    let mut body = order_body(draft)?;
    body["client_order_id"] = json!(client_id.as_str());
    Ok(body)
}

pub fn status(text: &str) -> OrderStatus {
    match text {
        "PENDING" | "QUEUED" => OrderStatus::Pending,
        "OPEN" => OrderStatus::Open,
        "FILLED" => OrderStatus::Filled,
        "CANCELLED" => OrderStatus::Cancelled,
        "EXPIRED" => OrderStatus::Expired,
        "FAILED" => OrderStatus::Failed,
        "CANCEL_QUEUED" => OrderStatus::CancelQueued,
        "EDIT_QUEUED" => OrderStatus::EditQueued,
        _ => OrderStatus::Unknown,
    }
}

/// The `order_status` values that mean an order may still trade.
pub const WORKING_STATUSES: [&str; 5] =
    ["OPEN", "PENDING", "QUEUED", "CANCEL_QUEUED", "EDIT_QUEUED"];

pub fn status_query(status: OrderStatus) -> &'static [&'static str] {
    match status {
        OrderStatus::Pending => &["PENDING", "QUEUED"],
        OrderStatus::Open => &["OPEN"],
        OrderStatus::Filled => &["FILLED"],
        OrderStatus::Cancelled => &["CANCELLED"],
        OrderStatus::Expired => &["EXPIRED"],
        OrderStatus::Failed => &["FAILED"],
        OrderStatus::CancelQueued => &["CANCEL_QUEUED"],
        OrderStatus::EditQueued => &["EDIT_QUEUED"],
        OrderStatus::Unknown => &["UNKNOWN_ORDER_STATUS"],
    }
}

fn order_type(dto: &OrderDto, configuration: &str) -> OrderType {
    match dto.order_type.as_str() {
        "MARKET" => OrderType::Market,
        "LIMIT" => OrderType::Limit,
        "STOP_LIMIT" => OrderType::StopLimit,
        "" => {
            if configuration.starts_with("market") {
                OrderType::Market
            } else if configuration.starts_with("stop_limit") {
                OrderType::StopLimit
            } else if configuration.starts_with("limit") || configuration.starts_with("sor") {
                OrderType::Limit
            } else {
                OrderType::Other
            }
        }
        _ => OrderType::Other,
    }
}

/// An order as Coinbase reports it. `None` when it lacks an id or a market.
pub fn order_from(dto: &OrderDto) -> Option<LiveOrder> {
    if dto.order_id.is_empty() || dto.product_id.is_empty() {
        return None;
    }
    let side = side_from(&dto.side)?;
    let (kind, config) = dto
        .order_configuration
        .as_object()
        .and_then(|map| map.iter().next())
        .map_or(("", &Value::Null), |(key, value)| (key.as_str(), value));
    let field = |name: &str| {
        config
            .get(name)
            .and_then(Value::as_str)
            .and_then(parse_decimal)
    };
    let end_ms = config
        .get("end_time")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_ms)
        // Coinbase writes "no end time" as the year 1.
        .filter(|ms| *ms > 0);
    let tif = match dto.time_in_force.as_str() {
        "GOOD_UNTIL_DATE_TIME" => Some(TimeInForce::GoodTilDate(end_ms.unwrap_or(0))),
        "GOOD_UNTIL_CANCELLED" => Some(TimeInForce::GoodTilCancelled),
        "IMMEDIATE_OR_CANCEL" => Some(TimeInForce::ImmediateOrCancel),
        "FILL_OR_KILL" => Some(TimeInForce::FillOrKill),
        _ => None,
    };
    let size = if dto.size_in_quote {
        field("quote_size").map(OrderSize::Quote)
    } else {
        field("base_size")
            .map(OrderSize::Base)
            .or_else(|| field("quote_size").map(OrderSize::Quote))
    };
    let stop_direction = match config.get("stop_direction").and_then(Value::as_str) {
        Some("STOP_DIRECTION_STOP_UP") => Some(StopDirection::Up),
        Some("STOP_DIRECTION_STOP_DOWN") => Some(StopDirection::Down),
        _ => None,
    };
    let reason = [&dto.reject_message, &dto.cancel_message]
        .into_iter()
        .find(|text| !text.is_empty())
        .cloned()
        .or_else(|| {
            (!dto.reject_reason.is_empty() && dto.reject_reason != "REJECT_REASON_UNSPECIFIED")
                .then(|| humanise(&dto.reject_reason))
        });
    let positive = |text: &str| parse_decimal(text).filter(|v| *v > Decimal::ZERO);
    Some(LiveOrder {
        id: OrderId::new(&dto.order_id),
        client_id: (!dto.client_order_id.is_empty())
            .then(|| ClientOrderId::new(&dto.client_order_id)),
        product: ProductId::new(&dto.product_id),
        side,
        order_type: order_type(dto, kind),
        status: status(&dto.status),
        tif,
        size,
        limit_price: field("limit_price").filter(|v| *v > Decimal::ZERO),
        stop_price: field("stop_price")
            .or_else(|| field("stop_trigger_price"))
            .filter(|v| *v > Decimal::ZERO),
        stop_direction,
        post_only: config
            .get("post_only")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        filled_size: parse_decimal(&dto.filled_size).unwrap_or(Decimal::ZERO),
        filled_value: parse_decimal(&dto.filled_value).unwrap_or(Decimal::ZERO),
        average_price: positive(&dto.average_filled_price),
        fees: parse_decimal(&dto.total_fees).unwrap_or(Decimal::ZERO),
        created_ms: parse_rfc3339_ms(&dto.created_time),
        updated_ms: dto
            .last_update_time
            .as_deref()
            .or(dto.last_fill_time.as_deref())
            .and_then(parse_rfc3339_ms)
            .or_else(|| parse_rfc3339_ms(&dto.created_time)),
        reason,
        stop_triggered: dto.trigger_status == "STOP_TRIGGERED",
    })
}

/// An order from the `user` WebSocket channel. That channel states the filled amount
/// (`cumulative_quantity`) and what is left (`leaves_quantity`), not the original size, so the size
/// is their sum. `now_ms` stamps the update (the channel carries no update time).
pub fn order_from_user(value: &Value, now_ms: i64) -> Option<LiveOrder> {
    let text = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("");
    let number = |key: &str| parse_decimal(text(key));
    let positive = |key: &str| number(key).filter(|v| *v > Decimal::ZERO);
    let id = text("order_id");
    let product = text("product_id");
    if id.is_empty() || product.is_empty() {
        return None;
    }
    let side = side_from(text("order_side")).or_else(|| side_from(text("side")))?;
    let filled = number("cumulative_quantity").unwrap_or(Decimal::ZERO);
    let size = number("leaves_quantity").map(|left| OrderSize::Base(filled + left));
    let end_ms = parse_rfc3339_ms(text("end_time")).filter(|ms| *ms > 0);
    let tif = match text("time_in_force") {
        "GOOD_UNTIL_DATE_TIME" => Some(TimeInForce::GoodTilDate(end_ms.unwrap_or(0))),
        "GOOD_UNTIL_CANCELLED" => Some(TimeInForce::GoodTilCancelled),
        "IMMEDIATE_OR_CANCEL" => Some(TimeInForce::ImmediateOrCancel),
        "FILL_OR_KILL" => Some(TimeInForce::FillOrKill),
        _ => None,
    };
    let reason = ["cancel_reason", "reject_reason"]
        .into_iter()
        .map(text)
        .find(|t| !t.is_empty() && *t != "REJECT_REASON_UNSPECIFIED" && *t != "Unspecified")
        .map(str::to_owned);
    Some(LiveOrder {
        id: OrderId::new(id),
        client_id: Some(text("client_order_id"))
            .filter(|t| !t.is_empty())
            .map(ClientOrderId::new),
        product: ProductId::new(product),
        side,
        order_type: match text("order_type") {
            "MARKET" => OrderType::Market,
            "LIMIT" => OrderType::Limit,
            "STOP_LIMIT" => OrderType::StopLimit,
            _ => OrderType::Other,
        },
        status: status(text("status")),
        tif,
        size,
        limit_price: positive("limit_price"),
        stop_price: positive("stop_price"),
        stop_direction: None,
        post_only: text("post_only") == "true",
        filled_size: filled,
        filled_value: number("filled_value").unwrap_or(Decimal::ZERO),
        average_price: positive("avg_price"),
        fees: number("total_fees").unwrap_or(Decimal::ZERO),
        created_ms: parse_rfc3339_ms(text("creation_time")),
        updated_ms: Some(now_ms),
        reason,
        stop_triggered: text("trigger_status") == "STOP_TRIGGERED",
    })
}

/// A fill. `None` when it is unusable (no id, no price or size).
pub fn fill_from(dto: &FillDto) -> Option<LiveFill> {
    let price = parse_decimal(&dto.price).filter(|v| *v > Decimal::ZERO)?;
    let raw = parse_decimal(&dto.size).filter(|v| *v > Decimal::ZERO)?;
    // Some fills state their size in the quote asset.
    let size = if dto.size_in_quote { raw / price } else { raw };
    let id = if dto.trade_id.is_empty() {
        &dto.entry_id
    } else {
        &dto.trade_id
    };
    if id.is_empty() || dto.order_id.is_empty() {
        return None;
    }
    Some(LiveFill {
        id: id.clone(),
        order_id: OrderId::new(&dto.order_id),
        product: ProductId::new(&dto.product_id),
        side: side_from(&dto.side)?,
        price,
        size,
        fee: parse_decimal(&dto.commission).unwrap_or(Decimal::ZERO),
        liquidity: match dto.liquidity_indicator.as_str() {
            "MAKER" => Some(Liquidity::Maker),
            "TAKER" => Some(Liquidity::Taker),
            _ => None,
        },
        time_ms: parse_rfc3339_ms(&dto.trade_time)?,
    })
}

/// `SOME_REASON_CODE` as a sentence, for codes with no hand-written text.
pub fn humanise(code: &str) -> String {
    let words = code
        .trim_start_matches("PREVIEW_")
        .replace('_', " ")
        .to_lowercase();
    let mut chars = words.chars();
    let sentence = chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    });
    format!("{sentence}.")
}

/// A sentence for a Coinbase order or preview failure code.
pub fn failure_text(code: &str) -> String {
    let code = code.trim_start_matches("PREVIEW_");
    match code {
        "INSUFFICIENT_FUND" | "INSUFFICIENT_FUNDS" | "REJECT_REASON_INSUFFICIENT_FUNDS" => {
            "Insufficient funds for this order."
        }
        "INVALID_SIZE_PRECISION" | "INVALID_BASE_SIZE_PRECISION" => {
            "The size has too many decimal places for this market."
        }
        "INVALID_PRICE_PRECISION" | "INVALID_LIMIT_PRICE_PRECISION" => {
            "The price has too many decimal places for this market."
        }
        "INVALID_QUOTE_SIZE_PRECISION" => "The amount has too many decimal places for this market.",
        "INVALID_BASE_SIZE_TOO_SMALL" | "SMALL_ORDER" => "The size is below the market's minimum.",
        "INVALID_QUOTE_SIZE_TOO_SMALL" => "The amount is below the market's minimum.",
        "INVALID_BASE_SIZE_TOO_LARGE" | "INVALID_QUOTE_SIZE_TOO_LARGE" => {
            "The order is above the market's maximum."
        }
        "INVALID_PRODUCT_ID"
        | "UNTRADABLE_PRODUCT"
        | "TRADING_DISABLED"
        | "PRODUCT_TRADING_HALTED"
        | "ORDER_ENTRY_DISABLED" => "Trading is not available in this market right now.",
        "GEOFENCING_RESTRICTION" => "Coinbase does not allow this trade from your region.",
        "INELIGIBLE_PAIR" => "This account cannot trade this pair.",
        "INVALID_LIMIT_PRICE_POST_ONLY" => {
            "A post-only order at this price would trade immediately, so it was refused."
        }
        "INVALID_LIMIT_PRICE"
        | "LIMIT_PRICE_TOO_FAR_FROM_MARKET"
        | "LIMIT_ORDER_PRICE_EXCEEDS_PRICE_BAND_ON_BUY"
        | "LIMIT_ORDER_PRICE_EXCEEDS_PRICE_BAND_ON_SELL" => {
            "The limit price is too far from the market price."
        }
        "INVALID_NO_LIQUIDITY" => "There is not enough liquidity to fill this order.",
        "FOK_DISABLED" => "Fill-or-kill orders are disabled for this market.",
        "STOP_ALREADY_TRIGGERED" => "The stop price has already been reached.",
        "STOP_PRICE_ABOVE_LAST_TRADE_PRICE" | "STOP_PRICE_BELOW_LAST_TRADE_PRICE" => {
            "The stop price is on the wrong side of the last trade price."
        }
        "END_TIME_TOO_FAR_IN_FUTURE" => "The expiry is too far in the future.",
        "DUPLICATE_CLIENT_ORDER_ID" => "An order with this id was already submitted.",
        "RATE_LIMIT_EXCEEDED" => "Coinbase is rate limiting this account. Wait a moment.",
        "TOO_MANY_OPEN_ORDERS" => "There are too many open orders.",
        "BIG_ORDER" => "This is a large order for the market.",
        "DURATION_EXTENDED_BY_MARKET_CLOSE" => "The order will run past the market's close.",
        _ => return humanise(code),
    }
    .to_owned()
}

/// A sentence for a Coinbase edit failure code, and whether it means "this order cannot be
/// edited" (as opposed to "this edit is not acceptable").
pub fn edit_failure(code: &str) -> (String, ErrorKind) {
    let (text, kind) = match code {
        "ONLY_OPEN_ORDERS_CAN_BE_EDITED" => {
            ("Only open orders can be edited.", ErrorKind::Unsupported)
        }
        "ONLY_LIMIT_ORDER_EDITS_SUPPORTED" | "ORDER_EDIT_ORDER_TYPE_NOT_SUPPORTED" => (
            "This type of order cannot be edited.",
            ErrorKind::Unsupported,
        ),
        "SIZE_IN_QUOTE_EDITS_NOT_ALLOWED" => (
            "Orders sized by an amount to spend cannot be edited.",
            ErrorKind::Unsupported,
        ),
        "CANNOT_EDIT_ORDER_PENDING_CANCEL" => {
            ("This order is being cancelled.", ErrorKind::Unsupported)
        }
        "ORDER_IS_ALREADY_BEING_REPLACED" => (
            "An edit of this order is already in progress.",
            ErrorKind::Rejected,
        ),
        "EXCEEDED_MAX_ALLOWED_EDIT_REQUEST_COUNT" => (
            "This order has been edited too many times.",
            ErrorKind::Unsupported,
        ),
        "EDIT_REQUEST_EQUAL_TO_ORIGINAL_REQUEST" => ("Nothing was changed.", ErrorKind::Invalid),
        "ORDER_NOT_FOUND" => ("The order was not found.", ErrorKind::NotFound),
        other => return (failure_text(other), ErrorKind::Rejected),
    };
    (text.to_owned(), kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_support::d;

    fn limit(tif: TimeInForce, post_only: bool) -> OrderDraft {
        OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Buy,
            kind: OrderKind::Limit {
                base_size: d("0.001"),
                price: d("90000.00"),
                tif,
                post_only,
            },
        }
    }

    #[test]
    fn drafts_become_the_documented_configurations_with_exact_digits() {
        let body = create_body(
            &limit(TimeInForce::GoodTilCancelled, true),
            &ClientOrderId::new("cid-1"),
        )
        .unwrap();
        assert_eq!(
            body,
            json!({
                "client_order_id": "cid-1",
                "product_id": "BTC-USD",
                "side": "BUY",
                "order_configuration": {"limit_limit_gtc": {
                    "base_size": "0.001", "limit_price": "90000.00", "post_only": true}}
            })
        );
        let gtd = order_configuration(&limit(TimeInForce::GoodTilDate(1_794_225_600_000), false))
            .unwrap();
        assert_eq!(
            gtd["limit_limit_gtd"]["end_time"],
            "2026-11-09T12:00:00.000Z"
        );
        let fok = order_configuration(&limit(TimeInForce::FillOrKill, false)).unwrap();
        assert!(fok["limit_limit_fok"].get("post_only").is_none());

        let market = |size| OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Sell,
            kind: OrderKind::Market { size },
        };
        assert_eq!(
            order_configuration(&market(OrderSize::Quote(d("25.00")))).unwrap(),
            json!({"market_market_ioc": {"quote_size": "25.00"}})
        );
        assert_eq!(
            order_configuration(&market(OrderSize::Base(d("0.5")))).unwrap(),
            json!({"market_market_ioc": {"base_size": "0.5"}})
        );
        let stop = OrderDraft {
            product: ProductId::new("BTC-USD"),
            side: Side::Sell,
            kind: OrderKind::StopLimit {
                base_size: d("0.01"),
                stop_price: d("85000.00"),
                limit_price: d("84990.00"),
                direction: StopDirection::Down,
                tif: TimeInForce::GoodTilCancelled,
            },
        };
        assert_eq!(
            order_configuration(&stop).unwrap(),
            json!({"stop_limit_stop_limit_gtc": {
                "base_size": "0.01", "limit_price": "84990.00",
                "stop_price": "85000.00", "stop_direction": "STOP_DIRECTION_STOP_DOWN"}})
        );
    }

    #[test]
    fn combinations_coinbase_cannot_take_are_refused_not_approximated() {
        assert_eq!(
            order_configuration(&limit(TimeInForce::ImmediateOrCancel, false))
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
        assert_eq!(
            order_configuration(&limit(TimeInForce::FillOrKill, true))
                .unwrap_err()
                .kind,
            ErrorKind::Invalid
        );
    }

    #[test]
    fn statuses_map_and_unknown_ones_stay_non_terminal() {
        for (text, expected) in [
            ("OPEN", OrderStatus::Open),
            ("PENDING", OrderStatus::Pending),
            ("QUEUED", OrderStatus::Pending),
            ("FILLED", OrderStatus::Filled),
            ("CANCELLED", OrderStatus::Cancelled),
            ("EXPIRED", OrderStatus::Expired),
            ("FAILED", OrderStatus::Failed),
            ("CANCEL_QUEUED", OrderStatus::CancelQueued),
            ("EDIT_QUEUED", OrderStatus::EditQueued),
            ("SOMETHING_NEW", OrderStatus::Unknown),
        ] {
            assert_eq!(status(text), expected, "{text}");
        }
        assert!(!status("SOMETHING_NEW").is_terminal());
    }

    fn dto(json: &str) -> OrderDto {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn a_resting_limit_order_is_read_in_full() {
        let order = order_from(&dto(
            r#"{"order_id":"o-1","product_id":"BTC-USD","client_order_id":"c-1","side":"BUY",
                "status":"OPEN","order_type":"LIMIT","time_in_force":"GOOD_UNTIL_CANCELLED",
                "created_time":"2026-11-09T12:00:00.100Z","filled_size":"0.0004",
                "filled_value":"36.00","average_filled_price":"90000.00","total_fees":"0.22",
                "order_configuration":{"limit_limit_gtc":{"base_size":"0.001","limit_price":"90000.00","post_only":true}}}"#,
        ))
        .unwrap();
        assert_eq!(order.id.as_str(), "o-1");
        assert_eq!(order.client_id.as_ref().unwrap().as_str(), "c-1");
        assert_eq!(order.order_type, OrderType::Limit);
        assert_eq!(order.status, OrderStatus::Open);
        assert_eq!(order.size, Some(OrderSize::Base(d("0.001"))));
        assert_eq!(order.limit_price, Some(d("90000.00")));
        assert!(order.post_only);
        assert_eq!(order.remaining_base(), Some(d("0.0006")));
        assert_eq!(order.average_price, Some(d("90000.00")));
        assert_eq!(order.fees, d("0.22"));
        assert_eq!(order.tif, Some(TimeInForce::GoodTilCancelled));
        assert_eq!(order.created_ms, Some(1_794_225_600_100));
    }

    #[test]
    fn stop_limits_quote_sized_markets_and_foreign_kinds_are_told_apart() {
        let stop = order_from(&dto(
            r#"{"order_id":"o","product_id":"BTC-USD","side":"SELL","status":"OPEN","order_type":"STOP_LIMIT",
                "time_in_force":"GOOD_UNTIL_DATE_TIME","trigger_status":"STOP_PENDING",
                "order_configuration":{"stop_limit_stop_limit_gtd":{"base_size":"0.01","limit_price":"84990","stop_price":"85000",
                "stop_direction":"STOP_DIRECTION_STOP_DOWN","end_time":"2026-12-01T00:00:00Z"}}}"#,
        ))
        .unwrap();
        assert_eq!(stop.order_type, OrderType::StopLimit);
        assert_eq!(stop.stop_direction, Some(StopDirection::Down));
        assert_eq!(stop.marker_price(), Some(d("85000")));
        assert!(matches!(stop.tif, Some(TimeInForce::GoodTilDate(ms)) if ms > 0));

        let market = order_from(&dto(
            r#"{"order_id":"o","product_id":"BTC-USD","side":"BUY","status":"FILLED","order_type":"MARKET",
                "size_in_quote":true,"order_configuration":{"market_market_ioc":{"quote_size":"25.00"}}}"#,
        ))
        .unwrap();
        assert_eq!(market.size, Some(OrderSize::Quote(d("25.00"))));
        assert_eq!(
            market.average_price, None,
            "no fills yet means no average price"
        );

        let bracket = order_from(&dto(
            r#"{"order_id":"o","product_id":"BTC-USD","side":"SELL","status":"OPEN","order_type":"BRACKET",
                "order_configuration":{"trigger_bracket_gtc":{"base_size":"1","limit_price":"100","stop_trigger_price":"90"}}}"#,
        ))
        .unwrap();
        assert_eq!(bracket.order_type, OrderType::Other);
        assert_eq!(bracket.stop_price, Some(d("90")));
    }

    #[test]
    fn rejections_carry_the_brokers_words_and_unusable_rows_are_dropped() {
        let failed = order_from(&dto(
            r#"{"order_id":"o","product_id":"BTC-USD","side":"BUY","status":"FAILED",
                "reject_reason":"REJECT_REASON_INSUFFICIENT_FUNDS","reject_message":"Not enough USD"}"#,
        ))
        .unwrap();
        assert_eq!(failed.reason.as_deref(), Some("Not enough USD"));
        let unspecified = order_from(&dto(
            r#"{"order_id":"o","product_id":"BTC-USD","side":"BUY","status":"OPEN","reject_reason":"REJECT_REASON_UNSPECIFIED"}"#,
        ))
        .unwrap();
        assert_eq!(unspecified.reason, None);
        assert!(order_from(&dto(r#"{"product_id":"BTC-USD","side":"BUY"}"#)).is_none());
        assert!(
            order_from(&dto(
                r#"{"order_id":"o","product_id":"BTC-USD","side":"HODL"}"#
            ))
            .is_none()
        );
    }

    #[test]
    fn fills_convert_quote_sized_amounts_and_need_the_essentials() {
        let fill = |json: &str| fill_from(&serde_json::from_str::<FillDto>(json).unwrap());
        let base = fill(
            r#"{"trade_id":"t1","order_id":"o","product_id":"BTC-USD","side":"BUY","price":"90000.00",
                "size":"0.001","commission":"0.54","liquidity_indicator":"MAKER","trade_time":"2026-11-09T12:00:01Z"}"#,
        )
        .unwrap();
        assert_eq!(base.size, d("0.001"));
        assert_eq!(base.liquidity, Some(Liquidity::Maker));
        assert_eq!(base.fee, d("0.54"));
        let quote = fill(
            r#"{"entry_id":"e1","order_id":"o","product_id":"BTC-USD","side":"SELL","price":"50000","size":"100",
                "size_in_quote":true,"trade_time":"2026-11-09T12:00:01Z"}"#,
        )
        .unwrap();
        assert_eq!(quote.size, d("0.002"));
        assert_eq!(quote.id, "e1");
        assert!(fill(r#"{"trade_id":"t","order_id":"o","side":"BUY","price":"0","size":"1","trade_time":"2026-11-09T12:00:01Z"}"#).is_none());
        assert!(fill(r#"{"trade_id":"t","order_id":"o","side":"BUY","price":"1","size":"1","trade_time":"soon"}"#).is_none());
    }

    #[test]
    fn user_channel_orders_derive_their_size_from_filled_plus_leaves() {
        let value: Value = serde_json::from_str(
            r#"{"order_id":"o","client_order_id":"c","product_id":"BTC-USD","order_side":"BUY",
                "order_type":"LIMIT","status":"OPEN","cumulative_quantity":"0.0004","leaves_quantity":"0.0006",
                "avg_price":"90000.00","limit_price":"90000.00","stop_price":"0","filled_value":"36",
                "total_fees":"0.2","post_only":"true","creation_time":"2026-11-09T12:00:00.100Z",
                "time_in_force":"GOOD_UNTIL_CANCELLED","end_time":"0001-01-01T00:00:00Z",
                "reject_reason":"REJECT_REASON_UNSPECIFIED","cancel_reason":"User requested cancel"}"#,
        )
        .unwrap();
        let order = order_from_user(&value, 777).unwrap();
        assert_eq!(order.size, Some(OrderSize::Base(d("0.0010"))));
        assert_eq!(order.filled_size, d("0.0004"));
        assert_eq!(order.limit_price, Some(d("90000.00")));
        assert_eq!(order.stop_price, None, "\"0\" means not applicable");
        assert!(order.post_only);
        assert_eq!(order.reason.as_deref(), Some("User requested cancel"));
        assert_eq!(order.updated_ms, Some(777));
        assert_eq!(order.created_ms, Some(1_794_225_600_100));
        assert!(order_from_user(&serde_json::json!({"product_id":"BTC-USD"}), 0).is_none());
    }

    #[test]
    fn failure_codes_have_sentences_and_unknown_ones_are_still_readable() {
        assert_eq!(
            failure_text("INSUFFICIENT_FUND"),
            "Insufficient funds for this order."
        );
        assert_eq!(
            failure_text("PREVIEW_INSUFFICIENT_FUND"),
            "Insufficient funds for this order."
        );
        assert_eq!(failure_text("SOME_NEW_REASON"), "Some new reason.");
        assert_eq!(
            edit_failure("ONLY_OPEN_ORDERS_CAN_BE_EDITED").1,
            ErrorKind::Unsupported
        );
        assert_eq!(edit_failure("ORDER_NOT_FOUND").1, ErrorKind::NotFound);
        assert_eq!(
            edit_failure("INVALID_PRICE_PRECISION").1,
            ErrorKind::Rejected
        );
    }
}
