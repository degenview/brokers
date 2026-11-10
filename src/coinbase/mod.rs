//! Coinbase Advanced Trade (spot). The only module that names Coinbase.
//!
//! Base `https://api.coinbase.com/api/v3/brokerage`. This is not the Coinbase Exchange ("pro")
//! API, which has different hosts, keys and shapes. Authentication is a CDP API key signed into a
//! short-lived JWT (see [`jwt`]).

mod connection;
mod descriptor;
#[cfg(any(test, feature = "testing"))]
pub mod fake;
pub mod jwt;
mod mapping;
mod orders;
mod stream;
mod time;
mod wire;

use crate::adapter::{Broker, Connection, Descriptor, Env, MarketRef, ProductResolution};
use crate::error::Result;
use crate::model::types::ProductId;
use crate::secret::Credentials;

pub use connection::CoinbaseConnection;
pub use descriptor::{
    DESCRIPTOR, FIELD_NAME, FIELD_PRIVATE_KEY, ID, SANDBOX_DESCRIPTOR, SANDBOX_ID,
};

/// The production API host.
pub(crate) const BASE_URL: &str = "https://api.coinbase.com";
/// Coinbase's sandbox host: canned answers for accounts and orders, no authentication.
pub(crate) const SANDBOX_URL: &str = "https://api-sandbox.coinbase.com";
/// Every Advanced Trade path starts with this.
pub(crate) const PREFIX: &str = "/api/v3/brokerage";

pub struct Coinbase;

impl Broker for Coinbase {
    fn descriptor(&self) -> &'static Descriptor {
        &DESCRIPTOR
    }

    fn resolve_market(&self, market: &MarketRef) -> ProductResolution {
        resolve(DESCRIPTOR.market_source, market)
    }

    fn connect(&self, credentials: &Credentials, env: &Env) -> Result<Box<dyn Connection>> {
        CoinbaseConnection::open(credentials, env).map(|c| Box::new(c) as Box<dyn Connection>)
    }
}

fn resolve(expected: &'static str, market: &MarketRef) -> ProductResolution {
    if !market.source.eq_ignore_ascii_case(expected) {
        return ProductResolution::OtherSource { expected };
    }
    match product_id_for_symbol(&market.symbol) {
        Some(id) => ProductResolution::Market(id),
        None => ProductResolution::Unrecognised,
    }
}

/// Coinbase's sandbox as a broker of its own: it trades the same markets as Coinbase, on canned
/// data, and needs no key.
pub struct CoinbaseSandbox;

impl Broker for CoinbaseSandbox {
    fn descriptor(&self) -> &'static Descriptor {
        &SANDBOX_DESCRIPTOR
    }

    fn resolve_market(&self, market: &MarketRef) -> ProductResolution {
        resolve(SANDBOX_DESCRIPTOR.market_source, market)
    }

    fn connect(&self, _credentials: &Credentials, env: &Env) -> Result<Box<dyn Connection>> {
        CoinbaseConnection::open_sandbox(env).map(|c| Box::new(c) as Box<dyn Connection>)
    }
}

/// `btc-usd`, `BTC/USD` and `BTC-USD` are the market `BTC-USD`. A bare `BTC` is read the way the
/// chart's market data reads it (as the USD market): it is the market being charted, so it is the
/// market that would trade, not a substitution. Anything else is not recognised.
pub(crate) fn product_id_for_symbol(symbol: &str) -> Option<ProductId> {
    let upper = symbol.trim().to_ascii_uppercase().replace('/', "-");
    if upper.is_empty() || !upper.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let mut parts = upper.split('-');
    let base = parts.next().filter(|p| !p.is_empty())?;
    match (parts.next(), parts.next()) {
        (None, _) => Some(ProductId::new(format!("{base}-USD"))),
        (Some(quote), None) if !quote.is_empty() => Some(ProductId::new(upper)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(source: &str, symbol: &str) -> ProductResolution {
        Coinbase.resolve_market(&MarketRef {
            source: source.to_owned(),
            symbol: symbol.to_owned(),
        })
    }

    #[test]
    fn coinbase_charts_resolve_to_the_same_market() {
        for (symbol, expected) in [
            ("BTC-USD", "BTC-USD"),
            ("btc-usd", "BTC-USD"),
            ("ETH/USDC", "ETH-USDC"),
            ("SOL", "SOL-USD"),
        ] {
            assert_eq!(
                resolve("coinbase", symbol),
                ProductResolution::Market(ProductId::new(expected)),
                "{symbol}"
            );
        }
        assert_eq!(
            resolve("COINBASE", "BTC-USD"),
            resolve("coinbase", "BTC-USD")
        );
    }

    #[test]
    fn other_sources_and_odd_symbols_are_never_substituted() {
        assert_eq!(
            resolve("binance", "BTCUSDT"),
            ProductResolution::OtherSource {
                expected: "coinbase"
            }
        );
        for bad in ["", "BTC-USD-PERP", "BTC USD", "BTC-", "-USD", "BTC_USD"] {
            assert_eq!(
                resolve("coinbase", bad),
                ProductResolution::Unrecognised,
                "{bad:?}"
            );
        }
    }
}

#[cfg(test)]
mod orders_tests;

#[cfg(test)]
mod stream_tests;

#[cfg(test)]
mod conformance_tests {
    use super::fake::CoinbaseExchange;
    use crate::testing::conformance::run_all;

    #[test]
    fn coinbase_passes_the_shared_conformance_suite() {
        run_all(|| Box::new(CoinbaseExchange::start()));
    }
}

#[cfg(test)]
mod leak_tests;
