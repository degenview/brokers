//! Which markets' best prices are being read, and when a price that was read may still be shown.
//!
//! A screen says it wants a market's prices every frame it draws them. They are read every couple
//! of seconds for as long as that goes on and not at all once it stops, so nothing polls for a chart
//! nobody is looking at. A price is **never shown once it is old**, by either of two clocks that must
//! both agree: how long it has sat here, and how old the exchange said it already was when it arrived.
//! A price delayed on the way is not new.
//!
//! Pure: no threads and no network. The caller reads the clock (`now` is a monotonic instant for how
//! long a price has been held; `wall_ms` is this computer's wall time, to compare with the time the
//! exchange stamped) and sends the requests [`QuoteBoard::due`] names. [`crate::BrokerRuntime`] does
//! exactly that for one connection.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::types::{ProductId, Quote};

/// How often a market's best prices are read while something is drawing them.
pub const READ_EVERY: Duration = Duration::from_secs(2);
/// A market nobody has asked for lately is no longer read.
pub const WANTED_FOR: Duration = Duration::from_secs(4);
/// A price older than this is not shown as a price.
pub const FRESH_FOR: Duration = Duration::from_secs(10);
/// A market nobody has asked for in this long is forgotten, and its last price with it.
pub const FORGET_AFTER: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Entry {
    /// The last price that arrived, and when.
    quote: Option<(Quote, Instant)>,
    /// A request for this market is out and not answered.
    in_flight: bool,
    asked_at: Option<Instant>,
    wanted_at: Option<Instant>,
}

/// The markets wanted at one connection and the prices read for them.
#[derive(Default)]
pub struct QuoteBoard {
    entries: HashMap<ProductId, Entry>,
}

/// Whether the exchange's own time on `quote` is further back than a price may be. A time in the
/// future (this computer's clock is behind) is not old: skew must not hide every price.
pub fn is_old(quote: &Quote, wall_ms: i64) -> bool {
    let age_ms = wall_ms.saturating_sub(quote.time_ms);
    age_ms > i64::try_from(FRESH_FOR.as_millis()).unwrap_or(i64::MAX)
}

impl QuoteBoard {
    /// Says that something on screen wants this market's prices. Cheap; call it every frame they
    /// are drawn.
    pub fn want(&mut self, product: &ProductId, now: Instant) {
        self.entries.entry(product.clone()).or_default().wanted_at = Some(now);
    }

    /// The market's best prices, when they are fresh. A price that is too old is `None`, never
    /// shown as if it were current.
    pub fn fresh(&self, product: &ProductId, now: Instant, wall_ms: i64) -> Option<Quote> {
        let (quote, at) = self.entries.get(product)?.quote.as_ref()?;
        (now.saturating_duration_since(*at) < FRESH_FOR && !is_old(quote, wall_ms))
            .then(|| quote.clone())
    }

    /// The markets to ask for now, each marked as asked: wanted lately, nothing already out, and not
    /// read in the last [`READ_EVERY`]. Call [`QuoteBoard::answered`] for each when it comes back.
    pub fn due(&mut self, now: Instant) -> Vec<ProductId> {
        let mut due = Vec::new();
        for (product, entry) in &mut self.entries {
            let wanted = entry
                .wanted_at
                .is_some_and(|at| now.saturating_duration_since(at) < WANTED_FOR);
            let rested = entry
                .asked_at
                .is_none_or(|at| now.saturating_duration_since(at) >= READ_EVERY);
            if wanted && !entry.in_flight && rested {
                entry.in_flight = true;
                entry.asked_at = Some(now);
                due.push(product.clone());
            }
        }
        due
    }

    /// A request came back. `answer` is `None` when it failed (the last price is kept, and ages out).
    /// An answer that was already old when it came is dropped, not stamped as new. Whether the price
    /// shown changed.
    pub fn answered(
        &mut self,
        product: &ProductId,
        answer: Option<Quote>,
        now: Instant,
        wall_ms: i64,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(product) else {
            return false;
        };
        entry.in_flight = false;
        match answer {
            Some(quote) if !is_old(&quote, wall_ms) => {
                entry.quote = Some((quote, now));
                true
            }
            _ => false,
        }
    }

    /// Forgets the markets nobody has asked for in a while, unless a request for one is still out.
    pub fn forget_idle(&mut self, now: Instant) {
        self.entries.retain(|_, entry| {
            entry.in_flight
                || entry
                    .wanted_at
                    .is_some_and(|at| now.saturating_duration_since(at) < FORGET_AFTER)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_support::d;

    const WALL: i64 = 1_000_000;

    fn product() -> ProductId {
        ProductId::new("BTC-USD")
    }

    fn quote(time_ms: i64) -> Quote {
        Quote {
            bid: Some(d("99")),
            ask: Some(d("101")),
            last: Some(d("100")),
            time_ms,
        }
    }

    #[test]
    fn a_market_nobody_wants_is_never_asked_for() {
        let t0 = Instant::now();
        let mut board = QuoteBoard::default();
        assert!(board.due(t0).is_empty());
        board.want(&product(), t0);
        assert_eq!(board.due(t0), [product()]);
        // Wanted once, not any more: after the window it is left alone.
        let mut board = QuoteBoard::default();
        board.want(&product(), t0);
        assert!(
            board
                .due(t0 + WANTED_FOR + Duration::from_millis(1))
                .is_empty()
        );
    }

    #[test]
    fn a_market_is_asked_again_only_when_the_last_request_is_answered_and_rested() {
        let t0 = Instant::now();
        let mut board = QuoteBoard::default();
        board.want(&product(), t0);
        assert_eq!(board.due(t0).len(), 1);
        // Still out: not asked twice, however long it takes.
        board.want(&product(), t0 + Duration::from_secs(3));
        assert!(board.due(t0 + Duration::from_secs(3)).is_empty());
        // Answered at 3 s: asked again once the read interval has passed since it was asked.
        let t3 = t0 + Duration::from_secs(3);
        board.answered(&product(), Some(quote(WALL)), t3, WALL);
        assert_eq!(
            board.due(t3).len(),
            1,
            "asked at 0 s, so 3 s is past the 2 s rest"
        );
        board.answered(&product(), Some(quote(WALL)), t3, WALL);
        board.want(&product(), t3);
        assert!(
            board.due(t3 + Duration::from_secs(1)).is_empty(),
            "asked 1 s ago: rest"
        );
    }

    #[test]
    fn a_price_is_fresh_until_it_has_been_held_too_long() {
        let t0 = Instant::now();
        let mut board = QuoteBoard::default();
        board.want(&product(), t0);
        board.due(t0);
        assert!(board.answered(&product(), Some(quote(WALL)), t0, WALL));
        assert!(
            board
                .fresh(&product(), t0 + Duration::from_secs(9), WALL)
                .is_some()
        );
        assert!(
            board.fresh(&product(), t0 + FRESH_FOR, WALL).is_none(),
            "held for the full window: no longer shown"
        );
    }

    #[test]
    fn a_price_the_exchange_stamped_long_ago_is_never_shown() {
        let t0 = Instant::now();
        let mut board = QuoteBoard::default();
        board.want(&product(), t0);
        board.due(t0);
        // Arrives now, but the exchange says it is 11 s old: dropped, not stamped as new.
        let stale = quote(WALL - 11_000);
        assert!(!board.answered(&product(), Some(stale), t0, WALL));
        assert!(board.fresh(&product(), t0, WALL).is_none());
        // Fresh when it arrived, old by the exchange's clock a while later.
        board.due(t0 + READ_EVERY);
        assert!(board.answered(&product(), Some(quote(WALL)), t0, WALL));
        assert!(board.fresh(&product(), t0, WALL).is_some());
        assert!(board.fresh(&product(), t0, WALL + 10_001).is_none());
    }

    #[test]
    fn a_time_in_the_future_is_not_old() {
        // This computer's clock is behind the exchange's: that must not hide every price.
        assert!(!is_old(&quote(WALL + 60_000), WALL));
        assert!(is_old(&quote(WALL - 10_001), WALL));
        assert!(!is_old(&quote(WALL - 10_000), WALL));
    }

    #[test]
    fn a_failed_read_keeps_the_last_price_until_it_ages_out_and_is_asked_for_again() {
        let t0 = Instant::now();
        let mut board = QuoteBoard::default();
        board.want(&product(), t0);
        board.due(t0);
        board.answered(&product(), Some(quote(WALL)), t0, WALL);
        let t2 = t0 + READ_EVERY;
        board.want(&product(), t2);
        assert_eq!(board.due(t2).len(), 1);
        assert!(!board.answered(&product(), None, t2, WALL));
        assert!(board.fresh(&product(), t2, WALL).is_some());
        assert!(board.fresh(&product(), t0 + FRESH_FOR, WALL).is_none());
        board.want(&product(), t2 + READ_EVERY);
        assert_eq!(
            board.due(t2 + READ_EVERY).len(),
            1,
            "a failure is not the end of asking"
        );
    }

    #[test]
    fn idle_markets_are_forgotten_but_one_with_a_request_out_is_kept() {
        let t0 = Instant::now();
        let mut board = QuoteBoard::default();
        let other = ProductId::new("ETH-USD");
        board.want(&product(), t0);
        board.want(&other, t0);
        board.due(t0);
        board.answered(&other, Some(quote(WALL)), t0, WALL);
        board.forget_idle(t0 + FORGET_AFTER);
        assert!(
            board.fresh(&other, t0, WALL).is_none(),
            "forgotten with its price"
        );
        // The one still out was kept: its answer is taken.
        assert!(board.answered(&product(), Some(quote(WALL)), t0, WALL));
    }
}
