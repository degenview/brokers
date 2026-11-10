//! The book: orders and fills as the broker last reported them, plus the local notes that a
//! request is in flight. One reducer for every broker.
//!
//! The broker is the source of truth. Nothing here deletes an order because the user clicked
//! cancel; an order leaves the working set only when the broker reports it finished. Updates arrive
//! from a stream and from REST reads, duplicated and out of order, so applying one is **idempotent**
//! and a stale one is ignored:
//!
//! * a finished order stays finished (but still learns a bigger fill, and its final fee and price
//!   from a read of the order itself);
//! * the filled amount never goes down;
//! * a bare `Pending` never undoes `Open`, and a bare stream `Open` never undoes `CancelQueued`.

use std::collections::{BTreeMap, HashSet};

use rust_decimal::Decimal;

use super::types::{LiveFill, LiveOrder, OrderId, OrderStatus};

/// Where an update came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// The listing read when (re)connecting: existing orders, not news.
    Snapshot,
    /// A live stream message.
    Stream,
    /// A single order read after the fact.
    Refetch,
}

/// A local note that something has been asked of the broker and not yet confirmed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayKind {
    CancelRequested,
    EditRequested {
        price: Option<Decimal>,
        size: Option<Decimal>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Overlay {
    pub kind: OverlayKind,
    pub since_ms: i64,
}

/// An order with its local note, if any.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrderView {
    pub order: LiveOrder,
    pub overlay: Option<Overlay>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoticeKind {
    Accepted,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    Expired,
}

/// Something worth telling the user, produced once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub order: LiveOrder,
}

#[derive(Debug, Default)]
pub struct Applied {
    pub changed: bool,
    pub notices: Vec<Notice>,
}

/// Whether `incoming` is news about an order we already hold, or an old message arriving late.
///
/// * The filled amount never goes down.
/// * A finished order never becomes unfinished. It still learns more: a bigger fill (a late
///   message can carry one) and, from a read of the order itself rather than a stream message, the
///   final fee, average price and value of the same ending, which a stream message may state
///   before they are final.
/// * `Open -> EditQueued -> Open` is a real life cycle (an edit settling), so that is never
///   stale. A cancel can be refused, so `CancelQueued -> Open` is possible too, but a *stream*
///   message that says nothing except `Open` after `CancelQueued` is far more likely to be an old
///   one arriving late. The order is read again after every cancel, and that read is believed.
fn is_stale(existing: &LiveOrder, incoming: &LiveOrder, source: Source) -> bool {
    if incoming.filled_size < existing.filled_size {
        return true;
    }
    if existing.status.is_terminal() {
        if !incoming.status.is_terminal() {
            return true;
        }
        if incoming.filled_size > existing.filled_size {
            return false;
        }
        let same_ending = incoming.status == existing.status;
        let learns = incoming.fees >= existing.fees
            && (incoming.fees != existing.fees
                || incoming.average_price != existing.average_price
                || incoming.filled_value != existing.filled_value);
        return !(source != Source::Stream && same_ending && learns);
    }
    if incoming.filled_size == existing.filled_size
        && incoming.status == OrderStatus::Pending
        && matches!(
            existing.status,
            OrderStatus::Open | OrderStatus::EditQueued | OrderStatus::CancelQueued
        )
    {
        return true;
    }
    if source == Source::Stream
        && existing.status == OrderStatus::CancelQueued
        && incoming.status.rank() < existing.status.rank()
    {
        let mut same_but_status = incoming.clone();
        same_but_status.status = existing.status;
        same_but_status.updated_ms = existing.updated_ms;
        same_but_status.reason.clone_from(&existing.reason);
        return same_but_status == *existing;
    }
    false
}

/// Finished orders kept for display after they leave the working set.
const KEEP_FINISHED: usize = 50;
const KEEP_FILLS: usize = 1000;
/// A local note older than this is dropped: the broker has had its say or never will.
pub const OVERLAY_MAX_AGE_MS: i64 = 60_000;

#[derive(Default)]
pub struct Book {
    orders: BTreeMap<OrderId, LiveOrder>,
    overlays: BTreeMap<OrderId, Overlay>,
    fills: Vec<LiveFill>,
    fill_ids: HashSet<String>,
    notified: HashSet<(OrderId, NoticeKind, String)>,
}

impl Book {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &OrderId) -> Option<&LiveOrder> {
        self.orders.get(id)
    }

    /// Applies one order update. Idempotent; a stale update changes nothing.
    pub fn apply_order(&mut self, incoming: LiveOrder, source: Source) -> Applied {
        let previous = self.orders.get(&incoming.id).cloned();
        if let Some(existing) = &previous
            && (is_stale(existing, &incoming, source) || *existing == incoming)
        {
            return Applied::default();
        }

        let mut notices = Vec::new();
        let was_known = previous.is_some();
        let before = previous
            .as_ref()
            .map_or(Decimal::ZERO, |order| order.filled_size);
        // News is anything not learned from the initial listing of a never-seen order.
        let is_news = was_known || source != Source::Snapshot;

        if is_news {
            if !was_known && !incoming.status.is_terminal() {
                self.note(&mut notices, NoticeKind::Accepted, &incoming, "");
            }
            if incoming.filled_size > before && incoming.status != OrderStatus::Filled {
                let key = incoming.filled_size.normalize().to_string();
                self.note(&mut notices, NoticeKind::PartiallyFilled, &incoming, &key);
            }
            let terminal = match incoming.status {
                OrderStatus::Filled => Some(NoticeKind::Filled),
                OrderStatus::Cancelled => Some(NoticeKind::Cancelled),
                OrderStatus::Failed => Some(NoticeKind::Rejected),
                OrderStatus::Expired => Some(NoticeKind::Expired),
                _ => None,
            };
            if let Some(kind) = terminal {
                self.note(&mut notices, kind, &incoming, "");
            }
        } else if !incoming.status.is_terminal() {
            // Seen for the first time in a listing: remember we have, tell no one.
            self.notified
                .insert((incoming.id.clone(), NoticeKind::Accepted, String::new()));
        }

        let id = incoming.id.clone();
        self.orders.insert(id.clone(), incoming);
        self.settle_overlay(&id);
        self.prune_finished();
        Applied {
            changed: true,
            notices,
        }
    }

    fn note(
        &mut self,
        notices: &mut Vec<Notice>,
        kind: NoticeKind,
        order: &LiveOrder,
        detail: &str,
    ) {
        if self
            .notified
            .insert((order.id.clone(), kind, detail.to_owned()))
        {
            notices.push(Notice {
                kind,
                order: order.clone(),
            });
        }
    }

    /// Replaces the working set with a fresh listing of open orders. Returns the ids of orders we
    /// believed working that the listing does not contain: they finished (or vanished) while we
    /// were not looking, and must be read individually to learn how.
    pub fn apply_open_listing(&mut self, listing: Vec<LiveOrder>) -> (Vec<Notice>, Vec<OrderId>) {
        let listed: HashSet<OrderId> = listing.iter().map(|o| o.id.clone()).collect();
        let mut notices = Vec::new();
        for order in listing {
            notices.extend(self.apply_order(order, Source::Snapshot).notices);
        }
        let missing = self
            .orders
            .values()
            .filter(|o| o.status.is_working() && !listed.contains(&o.id))
            .map(|o| o.id.clone())
            .collect();
        (notices, missing)
    }

    /// Records a fill. `true` when it was new.
    pub fn apply_fill(&mut self, fill: LiveFill) -> bool {
        if !self.fill_ids.insert(fill.id.clone()) {
            return false;
        }
        let at = self
            .fills
            .partition_point(|existing| existing.time_ms >= fill.time_ms);
        self.fills.insert(at, fill);
        if self.fills.len() > KEEP_FILLS
            && let Some(dropped) = self.fills.pop()
        {
            self.fill_ids.remove(&dropped.id);
        }
        true
    }

    pub fn fills(&self) -> &[LiveFill] {
        &self.fills
    }

    pub fn mark_cancel_requested(&mut self, id: &OrderId, now_ms: i64) {
        if self.orders.get(id).is_some_and(|o| o.status.is_working()) {
            self.overlays.insert(
                id.clone(),
                Overlay {
                    kind: OverlayKind::CancelRequested,
                    since_ms: now_ms,
                },
            );
        }
    }

    pub fn mark_edit_requested(
        &mut self,
        id: &OrderId,
        price: Option<Decimal>,
        size: Option<Decimal>,
        now_ms: i64,
    ) {
        if self.orders.get(id).is_some_and(|o| o.status.is_working()) {
            self.overlays.insert(
                id.clone(),
                Overlay {
                    kind: OverlayKind::EditRequested { price, size },
                    since_ms: now_ms,
                },
            );
        }
    }

    pub fn clear_overlay(&mut self, id: &OrderId) {
        self.overlays.remove(id);
    }

    /// Drops notes the broker never confirmed within [`OVERLAY_MAX_AGE_MS`].
    pub fn expire_overlays(&mut self, now_ms: i64) -> bool {
        let before = self.overlays.len();
        self.overlays
            .retain(|_, o| now_ms.saturating_sub(o.since_ms) < OVERLAY_MAX_AGE_MS);
        self.overlays.len() != before
    }

    fn settle_overlay(&mut self, id: &OrderId) {
        let Some(order) = self.orders.get(id) else {
            return;
        };
        let Some(overlay) = self.overlays.get(id) else {
            return;
        };
        let done = match overlay.kind {
            OverlayKind::CancelRequested => {
                order.status == OrderStatus::CancelQueued || order.status.is_terminal()
            }
            OverlayKind::EditRequested { price, size } => order.status.is_terminal()
                || (order.status == OrderStatus::Open
                    && price.is_none_or(|p| {
                        order.limit_price == Some(p) || order.stop_price == Some(p)
                    })
                    && size.is_none_or(
                        |s| matches!(order.size, Some(super::types::OrderSize::Base(b)) if b == s),
                    )),
        };
        if done {
            self.overlays.remove(id);
        }
    }

    fn prune_finished(&mut self) {
        let mut finished: Vec<(i64, OrderId)> = self
            .orders
            .values()
            .filter(|o| o.status.is_terminal())
            .map(|o| (o.updated_ms.or(o.created_ms).unwrap_or(0), o.id.clone()))
            .collect();
        if finished.len() <= KEEP_FINISHED {
            return;
        }
        finished.sort();
        for (_, id) in finished.iter().take(finished.len() - KEEP_FINISHED) {
            self.orders.remove(id);
            self.overlays.remove(id);
        }
    }

    /// Every order: working first (newest first), then finished (most recently updated first).
    pub fn views(&self) -> Vec<OrderView> {
        let mut views: Vec<OrderView> = self
            .orders
            .values()
            .map(|order| OrderView {
                order: order.clone(),
                overlay: self.overlays.get(&order.id).copied(),
            })
            .collect();
        views.sort_by(|a, b| {
            let rank = |v: &OrderView| u8::from(v.order.status.is_terminal());
            let key = |v: &OrderView| v.order.updated_ms.or(v.order.created_ms).unwrap_or(0);
            rank(a)
                .cmp(&rank(b))
                .then_with(|| key(b).cmp(&key(a)))
                .then_with(|| a.order.id.cmp(&b.order.id))
        });
        views
    }

    pub fn working_count(&self) -> usize {
        self.orders
            .values()
            .filter(|o| o.status.is_working())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::test_support::d;
    use crate::model::types::{OrderSize, OrderType, ProductId, Side, TimeInForce};

    fn order(id: &str, status: OrderStatus, filled: &str) -> LiveOrder {
        LiveOrder {
            id: OrderId::new(id),
            client_id: None,
            product: ProductId::new("BTC-USD"),
            side: Side::Buy,
            order_type: OrderType::Limit,
            status,
            tif: Some(TimeInForce::GoodTilCancelled),
            size: Some(OrderSize::Base(d("1"))),
            limit_price: Some(d("100")),
            stop_price: None,
            stop_direction: None,
            post_only: false,
            filled_size: d(filled),
            filled_value: Decimal::ZERO,
            average_price: None,
            fees: Decimal::ZERO,
            created_ms: Some(1_000),
            updated_ms: Some(1_000),
            reason: None,
            stop_triggered: false,
        }
    }

    fn kinds(applied: &Applied) -> Vec<NoticeKind> {
        applied.notices.iter().map(|n| n.kind).collect()
    }

    #[test]
    fn applying_the_same_update_twice_changes_nothing_the_second_time() {
        let mut book = Book::new();
        let first = book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        assert!(first.changed);
        assert_eq!(kinds(&first), [NoticeKind::Accepted]);
        let again = book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        assert!(!again.changed);
        assert!(again.notices.is_empty());
    }

    #[test]
    fn a_finished_order_stays_finished_and_the_fill_never_goes_down() {
        let mut book = Book::new();
        book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        book.apply_order(order("a", OrderStatus::Open, "0.5"), Source::Stream);
        // A late, older update.
        assert!(
            !book
                .apply_order(order("a", OrderStatus::Open, "0.2"), Source::Stream)
                .changed
        );
        assert_eq!(book.get(&OrderId::new("a")).unwrap().filled_size, d("0.5"));
        book.apply_order(order("a", OrderStatus::Cancelled, "0.5"), Source::Stream);
        // Out-of-order "still open" arriving after the cancel.
        assert!(
            !book
                .apply_order(order("a", OrderStatus::Open, "0.5"), Source::Stream)
                .changed
        );
        assert_eq!(
            book.get(&OrderId::new("a")).unwrap().status,
            OrderStatus::Cancelled
        );
    }

    #[test]
    fn a_bare_pending_does_not_undo_open_but_an_edit_may_round_trip() {
        let mut book = Book::new();
        book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        assert!(
            !book
                .apply_order(order("a", OrderStatus::Pending, "0"), Source::Stream)
                .changed
        );
        // Open -> EditQueued -> Open is a real life cycle, not a regression.
        assert!(
            book.apply_order(order("a", OrderStatus::EditQueued, "0"), Source::Stream)
                .changed
        );
        assert!(
            book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream)
                .changed
        );
    }

    fn with_fees(mut o: LiveOrder, fees: &str, average: &str) -> LiveOrder {
        o.fees = d(fees);
        o.average_price = Some(d(average));
        o.filled_value = d(average) * o.filled_size;
        o
    }

    #[test]
    fn a_finished_order_learns_its_final_fee_and_price_from_a_read_but_not_from_the_stream() {
        let mut book = Book::new();
        book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        // The stream says filled, with the fee as it knew it then.
        let first = book.apply_order(
            with_fees(order("a", OrderStatus::Filled, "1"), "0.10", "100"),
            Source::Stream,
        );
        assert_eq!(kinds(&first), [NoticeKind::Filled]);
        // Another stream message for the same ending changes nothing.
        assert!(
            !book
                .apply_order(
                    with_fees(order("a", OrderStatus::Filled, "1"), "0.20", "101"),
                    Source::Stream
                )
                .changed
        );
        // The order read itself has the final figures: they replace the early ones, quietly.
        let read = book.apply_order(
            with_fees(order("a", OrderStatus::Filled, "1"), "0.25", "100.5"),
            Source::Refetch,
        );
        assert!(read.changed);
        assert!(read.notices.is_empty(), "the fill was told once");
        let held = book.get(&OrderId::new("a")).unwrap();
        assert_eq!(held.fees, d("0.25"));
        assert_eq!(held.average_price, Some(d("100.5")));
        // A read with a *smaller* fee is the older picture.
        assert!(
            !book
                .apply_order(
                    with_fees(order("a", OrderStatus::Filled, "1"), "0.10", "100"),
                    Source::Refetch
                )
                .changed
        );
    }

    #[test]
    fn a_finished_order_takes_a_bigger_fill_but_never_a_smaller_one_or_a_different_ending() {
        let mut book = Book::new();
        book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        book.apply_order(order("a", OrderStatus::Cancelled, "0.5"), Source::Stream);
        // The cancel raced a fill: the order in fact filled completely.
        assert!(
            book.apply_order(order("a", OrderStatus::Filled, "1"), Source::Refetch)
                .changed
        );
        assert_eq!(
            book.get(&OrderId::new("a")).unwrap().status,
            OrderStatus::Filled
        );
        // Late, older messages cannot undo it.
        for late in [
            order("a", OrderStatus::Cancelled, "0.5"),
            order("a", OrderStatus::Cancelled, "1"),
            order("a", OrderStatus::Open, "1"),
        ] {
            assert!(
                !book.apply_order(late.clone(), Source::Refetch).changed,
                "{late:?}"
            );
        }
        assert_eq!(book.get(&OrderId::new("a")).unwrap().filled_size, d("1"));
    }

    #[test]
    fn a_stream_message_that_only_lowers_the_status_is_old_but_a_read_is_believed() {
        let mut book = Book::new();
        book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        book.apply_order(order("a", OrderStatus::CancelQueued, "0"), Source::Stream);
        // An "open" that was sent before the cancel arrives after it.
        assert!(
            !book
                .apply_order(order("a", OrderStatus::Open, "0"), Source::Stream)
                .changed
        );
        assert_eq!(
            book.get(&OrderId::new("a")).unwrap().status,
            OrderStatus::CancelQueued
        );
        // The order read after the request is the answer, even if the cancel was refused.
        assert!(
            book.apply_order(order("a", OrderStatus::Open, "0"), Source::Refetch)
                .changed
        );
        assert_eq!(
            book.get(&OrderId::new("a")).unwrap().status,
            OrderStatus::Open
        );
    }

    #[test]
    fn an_edit_settling_is_news_from_the_stream_because_the_terms_changed() {
        let mut book = Book::new();
        book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        book.apply_order(order("a", OrderStatus::EditQueued, "0"), Source::Stream);
        let mut settled = order("a", OrderStatus::Open, "0");
        settled.limit_price = Some(d("101"));
        assert!(book.apply_order(settled, Source::Stream).changed);
        assert_eq!(
            book.get(&OrderId::new("a")).unwrap().limit_price,
            Some(d("101"))
        );
    }

    #[test]
    fn notices_fire_once_per_event_even_when_replayed() {
        let mut book = Book::new();
        let mut all = Vec::new();
        for _ in 0..3 {
            for (status, filled) in [
                (OrderStatus::Open, "0"),
                (OrderStatus::Open, "0.4"),
                (OrderStatus::Open, "0.4"),
                (OrderStatus::Filled, "1"),
            ] {
                all.extend(
                    book.apply_order(order("a", status, filled), Source::Stream)
                        .notices,
                );
            }
        }
        let kinds: Vec<_> = all.iter().map(|n| n.kind).collect();
        assert_eq!(
            kinds,
            [
                NoticeKind::Accepted,
                NoticeKind::PartiallyFilled,
                NoticeKind::Filled
            ]
        );
    }

    #[test]
    fn an_initial_listing_is_not_news_but_later_changes_are() {
        let mut book = Book::new();
        let (notices, missing) = book.apply_open_listing(vec![
            order("a", OrderStatus::Open, "0.3"),
            order("b", OrderStatus::Open, "0"),
        ]);
        assert!(notices.is_empty() && missing.is_empty());
        let filled = book.apply_order(order("a", OrderStatus::Filled, "1"), Source::Stream);
        assert_eq!(kinds(&filled), [NoticeKind::Filled]);
    }

    #[test]
    fn orders_missing_from_a_relisting_are_reported_for_refetch_not_dropped() {
        let mut book = Book::new();
        book.apply_open_listing(vec![
            order("a", OrderStatus::Open, "0"),
            order("b", OrderStatus::Open, "0"),
        ]);
        let (_, missing) = book.apply_open_listing(vec![order("b", OrderStatus::Open, "0")]);
        assert_eq!(missing, vec![OrderId::new("a")]);
        assert_eq!(
            book.get(&OrderId::new("a")).unwrap().status,
            OrderStatus::Open,
            "still believed open"
        );
        // Reading it individually settles how it ended, and tells the user.
        let applied = book.apply_order(order("a", OrderStatus::Cancelled, "0"), Source::Refetch);
        assert_eq!(kinds(&applied), [NoticeKind::Cancelled]);
    }

    #[test]
    fn a_cancel_note_is_not_a_cancellation_and_clears_when_the_broker_answers() {
        let mut book = Book::new();
        book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        let id = OrderId::new("a");
        book.mark_cancel_requested(&id, 5_000);
        let view = &book.views()[0];
        assert_eq!(
            view.order.status,
            OrderStatus::Open,
            "the order is untouched"
        );
        assert!(matches!(
            view.overlay,
            Some(Overlay {
                kind: OverlayKind::CancelRequested,
                ..
            })
        ));
        assert_eq!(book.working_count(), 1);
        // An unrelated update does not clear the note.
        book.apply_order(order("a", OrderStatus::Open, "0.1"), Source::Stream);
        assert!(book.views()[0].overlay.is_some());
        book.apply_order(order("a", OrderStatus::CancelQueued, "0.1"), Source::Stream);
        assert!(book.views()[0].overlay.is_none());
    }

    #[test]
    fn an_order_filling_during_a_cancel_ends_filled_and_the_note_goes() {
        let mut book = Book::new();
        book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        let id = OrderId::new("a");
        book.mark_cancel_requested(&id, 5_000);
        let applied = book.apply_order(order("a", OrderStatus::Filled, "1"), Source::Stream);
        assert_eq!(kinds(&applied), [NoticeKind::Filled]);
        assert!(book.views()[0].overlay.is_none());
        assert_eq!(book.working_count(), 0);
    }

    #[test]
    fn an_edit_note_clears_when_the_new_terms_appear_and_unconfirmed_notes_expire() {
        let mut book = Book::new();
        book.apply_order(order("a", OrderStatus::Open, "0"), Source::Stream);
        let id = OrderId::new("a");
        book.mark_edit_requested(&id, Some(d("99")), None, 1_000);
        book.apply_order(order("a", OrderStatus::EditQueued, "0"), Source::Stream);
        assert!(book.views()[0].overlay.is_some());
        let mut settled = order("a", OrderStatus::Open, "0");
        settled.limit_price = Some(d("99"));
        book.apply_order(settled, Source::Stream);
        assert!(book.views()[0].overlay.is_none());

        book.mark_cancel_requested(&id, 1_000);
        assert!(!book.expire_overlays(1_000 + OVERLAY_MAX_AGE_MS - 1));
        assert!(book.expire_overlays(1_000 + OVERLAY_MAX_AGE_MS));
        assert!(book.views()[0].overlay.is_none());
    }

    #[test]
    fn fills_are_deduplicated_ordered_and_bounded() {
        let mut book = Book::new();
        let fill = |id: &str, time: i64| LiveFill {
            id: id.to_owned(),
            order_id: OrderId::new("a"),
            product: ProductId::new("BTC-USD"),
            side: Side::Buy,
            price: d("100"),
            size: d("0.1"),
            fee: Decimal::ZERO,
            liquidity: None,
            time_ms: time,
        };
        assert!(book.apply_fill(fill("t2", 20)));
        assert!(book.apply_fill(fill("t1", 10)));
        assert!(book.apply_fill(fill("t3", 30)));
        assert!(
            !book.apply_fill(fill("t2", 20)),
            "a replayed fill is ignored"
        );
        let ids: Vec<_> = book.fills().iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["t3", "t2", "t1"], "newest first");
        for n in 0..(KEEP_FILLS + 10) {
            book.apply_fill(fill(&format!("x{n}"), 100 + n as i64));
        }
        assert_eq!(book.fills().len(), KEEP_FILLS);
    }

    #[test]
    fn views_list_working_orders_first_and_finished_orders_are_bounded() {
        let mut book = Book::new();
        let mut finished = order("done", OrderStatus::Filled, "1");
        finished.updated_ms = Some(9_000);
        book.apply_order(finished, Source::Refetch);
        let mut recent = order("new", OrderStatus::Open, "0");
        recent.created_ms = Some(5_000);
        recent.updated_ms = Some(5_000);
        book.apply_order(recent, Source::Stream);
        book.apply_order(order("old", OrderStatus::Open, "0"), Source::Stream);
        let ids: Vec<_> = book.views().iter().map(|v| v.order.id.0.clone()).collect();
        assert_eq!(ids, ["new", "old", "done"]);

        for n in 0..(KEEP_FINISHED + 5) {
            let mut o = order(&format!("f{n}"), OrderStatus::Cancelled, "0");
            o.updated_ms = Some(10_000 + n as i64);
            book.apply_order(o, Source::Refetch);
        }
        let terminal = book
            .views()
            .iter()
            .filter(|v| v.order.status.is_terminal())
            .count();
        assert_eq!(terminal, KEEP_FINISHED);
    }

    #[test]
    fn a_large_book_is_cheap_to_view() {
        let mut book = Book::new();
        for n in 0..500 {
            let mut o = order(&format!("o{n:04}"), OrderStatus::Open, "0");
            o.created_ms = Some(1_000 + n as i64);
            book.apply_order(o, Source::Stream);
        }
        let started = std::time::Instant::now();
        for _ in 0..20 {
            assert_eq!(book.views().len(), 500);
        }
        // Even in a debug build, 500 orders must not cost a visible fraction of a frame.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }
}
