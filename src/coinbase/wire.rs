//! Coinbase Advanced Trade JSON shapes (docs.cdp.coinbase.com OpenAPI spec, read 2026-11-09).
//! Every field is optional on our side: a missing field never fails a whole page, and numbers
//! arrive as strings and are parsed with [`parse_decimal`] at the edge.

use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

/// `"0.0001"`, `"1E-8"` or `""` (absent).
pub fn parse_decimal(text: &str) -> Option<Decimal> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Decimal::from_str(text)
        .or_else(|_| Decimal::from_scientific(text))
        .ok()
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct KeyPermissions {
    pub can_view: bool,
    pub can_trade: bool,
    pub can_transfer: bool,
    pub portfolio_uuid: String,
    pub portfolio_type: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Amount {
    pub value: String,
    pub currency: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AccountDto {
    pub uuid: String,
    pub name: String,
    pub currency: String,
    pub available_balance: Amount,
    pub hold: Amount,
    pub active: bool,
    pub retail_portfolio_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct AccountsPage {
    pub accounts: Vec<AccountDto>,
    pub has_next: bool,
    pub cursor: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ProductDto {
    pub product_id: String,
    pub base_currency_id: String,
    pub quote_currency_id: String,
    pub display_name: String,
    pub price_increment: String,
    pub base_increment: String,
    pub quote_increment: String,
    pub base_min_size: String,
    pub base_max_size: String,
    pub quote_min_size: String,
    pub quote_max_size: String,
    pub status: String,
    pub product_type: String,
    pub trading_disabled: bool,
    pub is_disabled: bool,
    pub cancel_only: bool,
    pub limit_only: bool,
    pub post_only: bool,
    pub auction_mode: bool,
    pub view_only: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ProductsPage {
    pub products: Vec<ProductDto>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Level {
    pub price: String,
    pub size: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PriceBook {
    pub product_id: String,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub time: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct BestBidAsk {
    pub pricebooks: Vec<PriceBook>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_parse_plain_scientific_and_blank() {
        assert_eq!(
            parse_decimal("0.00000001").unwrap().to_string(),
            "0.00000001"
        );
        assert_eq!(parse_decimal("1E-8").unwrap().to_string(), "0.00000001");
        assert_eq!(parse_decimal(" 12.50 ").unwrap().to_string(), "12.50");
        assert_eq!(parse_decimal(""), None);
        assert_eq!(parse_decimal("abc"), None);
    }

    #[test]
    fn missing_fields_default_instead_of_failing_the_page() {
        let page: AccountsPage = serde_json::from_str(
            r#"{"accounts":[{"uuid":"a1","currency":"BTC","available_balance":{"value":"1.5","currency":"BTC"}}],"has_next":false,"unknown_field":1}"#,
        )
        .unwrap();
        assert_eq!(page.accounts[0].hold.value, "");
        assert_eq!(page.accounts[0].available_balance.value, "1.5");
    }
}

// ---- Orders ------------------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct OrderDto {
    pub order_id: String,
    pub product_id: String,
    pub client_order_id: String,
    pub side: String,
    pub status: String,
    pub order_type: String,
    pub time_in_force: String,
    pub created_time: String,
    pub last_fill_time: Option<String>,
    pub last_update_time: Option<String>,
    pub filled_size: String,
    pub filled_value: String,
    pub average_filled_price: String,
    pub total_fees: String,
    pub size_in_quote: bool,
    pub trigger_status: String,
    pub reject_reason: String,
    pub reject_message: String,
    pub cancel_message: String,
    pub order_configuration: serde_json::Value,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct OrdersPage {
    pub orders: Vec<OrderDto>,
    pub has_next: bool,
    pub cursor: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct OrderEnvelope {
    pub order: OrderDto,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SuccessResponse {
    pub order_id: String,
    pub product_id: String,
    pub side: String,
    pub client_order_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ErrorResponse {
    pub error: String,
    pub message: String,
    pub error_details: String,
    pub preview_failure_reason: String,
    pub new_order_failure_reason: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CreateOrderResponse {
    pub success: bool,
    pub success_response: SuccessResponse,
    pub error_response: ErrorResponse,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct PreviewResponse {
    pub order_total: String,
    pub commission_total: String,
    pub errs: Vec<String>,
    pub warning: Vec<String>,
    pub quote_size: String,
    pub base_size: String,
    pub best_bid: String,
    pub best_ask: String,
    pub slippage: String,
    pub est_average_filled_price: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CancelEntry {
    pub success: bool,
    pub failure_reason: String,
    pub order_id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CancelResponse {
    pub results: Vec<CancelEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EditError {
    pub edit_failure_reason: String,
    pub preview_failure_reason: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EditResponse {
    pub success: bool,
    pub errors: Vec<EditError>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct EditPreviewResponse {
    pub errors: Vec<EditError>,
    pub order_total: String,
    pub commission_total: String,
    pub quote_size: String,
    pub base_size: String,
    pub best_bid: String,
    pub best_ask: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FillDto {
    pub entry_id: String,
    pub trade_id: String,
    pub order_id: String,
    pub trade_time: String,
    pub price: String,
    pub size: String,
    pub commission: String,
    pub product_id: String,
    pub liquidity_indicator: String,
    pub size_in_quote: bool,
    pub side: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FillsPage {
    pub fills: Vec<FillDto>,
    pub cursor: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FeeTier {
    pub pricing_tier: String,
    pub maker_fee_rate: String,
    pub taker_fee_rate: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TransactionSummary {
    pub fee_tier: FeeTier,
}
