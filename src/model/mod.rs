//! The pure trading model: no network, no clock, no threads. Everything here is tested without a
//! broker.

pub mod capabilities;
pub mod edit;
pub mod events;
pub mod orders;
pub mod pending;
pub mod preview;
pub mod reducer;
pub mod types;
pub mod validate;

#[cfg(test)]
pub(crate) mod test_support {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    pub fn d(text: &str) -> Decimal {
        Decimal::from_str(text).expect("a test decimal")
    }
}
