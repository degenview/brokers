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
