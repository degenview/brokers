//! What a spot account is worth, as far as the prices at hand say.
//!
//! An account is a list of [`Balance`]s. Its value is each balance times a price in one chosen
//! currency; an asset with no price is left out and counted, never guessed, so the figure is an
//! estimate that says what it leaves out. Pure functions over [`Decimal`]: the caller supplies the
//! balances and a lookup for prices. Nothing here rounds.

use rust_decimal::Decimal;

use super::types::{Balance, Quote};

/// Cash assets, in the order they are preferred when two hold the same amount.
const CASH: [&str; 6] = ["USD", "USDC", "EUR", "GBP", "USDT", "DAI"];

/// The account's value in one currency.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Valuation {
    /// The currency every figure is in.
    pub currency: String,
    /// Everything that could be priced, cash included.
    pub equity: Decimal,
    /// The valuation currency that is free to spend.
    pub cash: Decimal,
    /// Priced value committed to open orders.
    pub on_hold: Decimal,
    /// Priced value of everything that is not the valuation currency.
    pub holdings: Decimal,
    /// Assets held with no price, sorted.
    pub unpriced: Vec<String>,
    /// How many assets with a balance there are, the unpriced included.
    pub assets: usize,
}

/// The currency to value in: the cash asset with the most of it, `USD` when there is none.
pub fn valuation_currency(balances: &[Balance]) -> String {
    CASH.iter()
        .filter_map(|code| {
            let total = balances
                .iter()
                .find(|balance| balance.asset == *code)
                .map(Balance::total)?;
            (total > Decimal::ZERO).then_some((*code, total))
        })
        // `max_by` keeps the last of equals; the list is walked in order of preference, so walk it
        // backwards to keep the first.
        .rev()
        .max_by(|a, b| a.1.cmp(&b.1))
        .map_or_else(|| "USD".to_owned(), |(code, _)| code.to_owned())
}

/// Values `balances` in `currency`. `price` gives the price of one unit of an asset in `currency`;
/// a price that is not above zero is no price.
pub fn value(
    balances: &[Balance],
    currency: &str,
    price: impl Fn(&str) -> Option<Decimal>,
) -> Valuation {
    let mut valuation = Valuation {
        currency: currency.to_owned(),
        equity: Decimal::ZERO,
        cash: Decimal::ZERO,
        on_hold: Decimal::ZERO,
        holdings: Decimal::ZERO,
        unpriced: Vec::new(),
        assets: 0,
    };
    for balance in balances.iter().filter(|b| b.total() > Decimal::ZERO) {
        valuation.assets += 1;
        let unit = if balance.asset == currency {
            Some(Decimal::ONE)
        } else {
            price(&balance.asset).filter(|p| *p > Decimal::ZERO)
        };
        let Some(unit) = unit else {
            valuation.unpriced.push(balance.asset.clone());
            continue;
        };
        valuation.equity += balance.total() * unit;
        valuation.on_hold += balance.held * unit;
        if balance.asset == currency {
            valuation.cash += balance.available;
        } else {
            valuation.holdings += balance.total() * unit;
        }
    }
    valuation.unpriced.sort();
    valuation
}

impl Quote {
    /// The price to mark a holding at: halfway between the best bid and ask when both are there,
    /// otherwise the last trade.
    pub fn mark(&self) -> Option<Decimal> {
        match (self.bid, self.ask) {
            (Some(bid), Some(ask)) => Some((bid + ask) / Decimal::TWO),
            _ => self.last,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_support::d;
    use crate::model::types::AccountId;

    fn balance(asset: &str, available: &str, held: &str) -> Balance {
        Balance {
            account: AccountId::new("a"),
            asset: asset.to_owned(),
            available: d(available),
            held: d(held),
        }
    }

    fn prices(asset: &str) -> Option<Decimal> {
        match asset {
            "BTC" => Some(d("60000")),
            "ETH" => Some(d("3000")),
            _ => None,
        }
    }

    #[test]
    fn the_currency_is_the_cash_asset_with_the_most_of_it() {
        let held = [
            balance("USD", "100", "0"),
            balance("EUR", "900", "50"),
            balance("BTC", "5", "0"),
        ];
        assert_eq!(valuation_currency(&held), "EUR");
        assert_eq!(valuation_currency(&[balance("BTC", "1", "0")]), "USD");
        assert_eq!(valuation_currency(&[]), "USD");
        // A tie goes to the one listed first.
        let tie = [balance("USDC", "10", "0"), balance("USD", "10", "0")];
        assert_eq!(valuation_currency(&tie), "USD");
    }

    #[test]
    fn value_is_each_balance_at_its_price_and_cash_counts_one_to_one() {
        let held = [
            balance("USD", "1000", "200"),
            balance("BTC", "0.5", "0.1"),
            balance("ETH", "2", "0"),
        ];
        let valuation = value(&held, "USD", prices);
        // 1200 cash + 0.6 BTC (36000) + 2 ETH (6000)
        assert_eq!(valuation.equity, d("43200"));
        assert_eq!(valuation.cash, d("1000"));
        assert_eq!(valuation.holdings, d("42000"));
        // 200 USD and 0.1 BTC (6000) are committed to orders.
        assert_eq!(valuation.on_hold, d("6200"));
        assert_eq!(valuation.assets, 3);
        assert!(valuation.unpriced.is_empty());
    }

    #[test]
    fn an_asset_without_a_price_is_left_out_and_counted() {
        let held = [
            balance("USD", "10", "0"),
            balance("BONK", "5000000", "0"),
            balance("ADA", "100", "0"),
            balance("BTC", "0", "0"),
        ];
        let valuation = value(&held, "USD", prices);
        assert_eq!(valuation.equity, d("10"));
        assert_eq!(valuation.unpriced, ["ADA", "BONK"]);
        // An empty balance is not an asset held.
        assert_eq!(valuation.assets, 3);
    }

    #[test]
    fn a_zero_or_negative_price_is_no_price() {
        let held = [balance("BTC", "1", "0")];
        assert_eq!(value(&held, "USD", |_| Some(d("0"))).unpriced, ["BTC"]);
        assert_eq!(value(&held, "USD", |_| Some(d("-1"))).unpriced, ["BTC"]);
    }

    #[test]
    fn a_quote_is_marked_at_the_middle_or_the_last_trade() {
        let both = Quote {
            bid: Some(d("99")),
            ask: Some(d("101")),
            last: Some(d("50")),
            time_ms: 0,
        };
        assert_eq!(both.mark(), Some(d("100")));
        let one_side = Quote {
            bid: Some(d("99")),
            last: Some(d("98")),
            ..Quote::default()
        };
        assert_eq!(one_side.mark(), Some(d("98")));
        assert_eq!(Quote::default().mark(), None);
    }
}
