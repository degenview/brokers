//! The brokers this build knows. Adding a broker means adding its module and one line here; no
//! other layer matches on a broker.

use crate::adapter::Broker;
use crate::model::types::BrokerId;

/// Every broker compiled into this build, in the order they should be listed.
pub fn all() -> Vec<&'static dyn Broker> {
    vec![
        #[cfg(feature = "coinbase")]
        &crate::coinbase::Coinbase,
        #[cfg(feature = "coinbase")]
        &crate::coinbase::CoinbaseSandbox,
    ]
}

/// The broker with this id, if it is compiled in.
pub fn by_id(id: BrokerId) -> Option<&'static dyn Broker> {
    all()
        .into_iter()
        .find(|broker| broker.descriptor().id == id)
}

/// The interned id for a stored string, if a broker by that name is compiled in.
pub fn id_for(text: &str) -> Option<BrokerId> {
    all()
        .into_iter()
        .map(|broker| broker.descriptor().id)
        .find(|id| id.as_str() == text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_round_trip_through_text() {
        let brokers = all();
        for broker in &brokers {
            let id = broker.descriptor().id;
            assert_eq!(id_for(id.as_str()), Some(id));
            assert!(by_id(id).is_some());
            assert!(
                id.as_str()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{id} must be lower-case ascii"
            );
        }
        let mut ids: Vec<_> = brokers.iter().map(|b| b.descriptor().id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), brokers.len());
        assert_eq!(id_for("nope"), None);
    }
}
