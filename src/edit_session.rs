//! Editing an open order in place: the state machine behind an Edit screen.
//!
//! An [`EditSession`] is the whole life of one edit: what is typed, the broker's preview of exactly
//! that change, the review the user approves and the amend that follows. The interface hands it a
//! fresh [`EditFrame`] and [`Entry`] every frame (the order as the broker has it now, the account and
//! connection, the market's rules, what is typed), draws the [`EditState`] it reports and passes back
//! what the user pressed as [`EditEvent`]s. Nothing here draws or waits: time is a parameter and the
//! broker's answers are polled [`Pending`]s. Every reason is a value; the words belong to the
//! interface.
//!
//! The rules it keeps, each a transition with a test:
//!
//! * A change is sent only after the broker's preview of **exactly that change, on exactly that
//!   order, on this connection**, and only while that preview is younger than [`PREVIEW_FRESH`].
//! * A review stands only while the order is unchanged: a fill, an edit made elsewhere, another
//!   account or another connection drops it back to editing and nothing is sent.
//! * The amend carries the fingerprint of the order that was reviewed, which the runtime checks again
//!   before anything leaves.

use std::time::{Duration, Instant};

use rust_decimal::Decimal;

use crate::error::{BrokerError, ErrorKind};
use crate::model::edit::{EditIssue, edit_issues};
use crate::model::orders::{EditOutcome, EditPreview, OrderChange, OrderFingerprint};
use crate::model::preview::{
    PREVIEW_DEBOUNCE, PREVIEW_FRESH, PreviewAnswer, PreviewGate, preview_gate,
};
use crate::model::ticket::NotANumber;
use crate::model::types::{
    AccountId, LiveOrder, OrderId, OrderSize, OrderStatus, OrderType, ProductSpec,
};
use crate::runtime::{BrokerRuntime, Pending};

/// What an edit asks of a connection. [`BrokerRuntime`] is one; a test supplies its own.
pub trait EditSource {
    /// Asks what the change would do, without applying it.
    fn preview_edit(&self, order: OrderId, change: OrderChange) -> Pending<EditPreview>;

    /// Applies the change, but only to the order `base` describes.
    fn amend(
        &self,
        order: OrderId,
        change: OrderChange,
        base: OrderFingerprint,
    ) -> Pending<EditOutcome>;
}

impl EditSource for BrokerRuntime {
    fn preview_edit(&self, order: OrderId, change: OrderChange) -> Pending<EditPreview> {
        BrokerRuntime::preview_edit(self, order, change)
    }

    fn amend(
        &self,
        order: OrderId,
        change: OrderChange,
        base: OrderFingerprint,
    ) -> Pending<EditOutcome> {
        BrokerRuntime::amend(self, order, change, base)
    }
}

/// What the world is this frame.
pub struct EditFrame<'a> {
    pub now: Instant,
    /// The order as the broker has it now; `None` once it is gone.
    pub live: Option<&'a LiveOrder>,
    /// The account the orders go to; `None` while there is none.
    pub account: Option<&'a AccountId>,
    /// Which connection the account is read on; a preview says nothing about another one.
    pub generation: u64,
    pub spec: Option<&'a ProductSpec>,
}

/// What is typed, already read: `None` is an empty box. The price box is the stop price of a
/// stop-limit order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry {
    pub price: Result<Option<Decimal>, NotANumber>,
    pub size: Result<Option<Decimal>, NotANumber>,
}

/// What the user did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditEvent {
    /// "Review change": show the change for approval.
    Review,
    /// "Back" from the approval.
    Back,
    /// "Change order" in the approval: send it.
    Confirm,
    /// "Try again" after a failed preview.
    TryAgain,
    /// "Cancel" or Escape: close without sending (ignored while an amend is in flight).
    Cancel,
}

/// Why the session ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Closing {
    /// The user left without sending anything.
    Dismissed,
    /// The order is no longer open, so it was not edited.
    NotOpen,
    /// There is no account (the connection is gone), so the order was not edited.
    NotConnected,
    /// The amend was sent; the order updates when the broker confirms it.
    Sent,
    /// The connection dropped before the amend was answered: the order is read again, not retried.
    OutcomeUnknown(BrokerError),
}

#[derive(Debug)]
pub enum Step {
    Open,
    Closed(Closing),
}

/// Why what is typed is not a change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotAChange {
    PriceNotANumber,
    SizeNotANumber,
    /// Valid, but the same as the order: nothing to review.
    Unchanged,
}

/// Something to tell the user while the session stays open.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditNote {
    /// The preview carried no fingerprint of the order, so the change was not sent.
    NotChecked,
    /// The order changed while it was being reviewed; nothing was sent.
    OrderChanged,
    /// The broker's check of the change is too old to act on.
    PreviewOutOfDate,
    /// The amend failed and the order was not changed.
    AmendFailed(BrokerError),
}

/// The change laid out for approval.
pub struct Review<'a> {
    pub change: &'a OrderChange,
    pub preview: &'a EditPreview,
}

/// What to draw for the last frame.
pub struct EditState<'a> {
    /// The order as the broker last reported it.
    pub order: &'a LiveOrder,
    /// `Some` while the change is up for approval.
    pub review: Option<Review<'a>>,
    pub change: &'a Result<OrderChange, NotAChange>,
    /// What the market's rules say is wrong with the change.
    pub issues: &'a [EditIssue],
    /// The broker's last answer to a preview.
    pub preview: Option<&'a EditPreview>,
    pub gate: &'a PreviewGate,
    /// Waiting on the broker's preview of a valid change.
    pub asking: bool,
    pub note: Option<&'a EditNote>,
    /// Whether "Review change" is enabled.
    pub can_review: bool,
    pub sending: bool,
}

/// Everything a preview is a statement about. A preview answers one of these and no other.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Asked {
    change: OrderChange,
    order: OrderFingerprint,
    generation: u64,
}

enum Stage {
    Editing,
    /// The change, and the broker's preview of exactly it.
    Confirming(OrderChange, Box<EditPreview>),
    Sending(Pending<EditOutcome>),
}

/// The broker's preview of what is typed, and when to ask for it.
struct Preview {
    /// What was last asked about; `None` while what is typed is not a valid change.
    seen: Option<Asked>,
    /// When `seen` last changed: the question waits until typing pauses.
    changed_at: Instant,
    pending: Option<(Asked, Pending<EditPreview>)>,
    answer: Option<EditPreview>,
    failure: Option<String>,
    /// What the answer or failure is about.
    answered_for: Option<Asked>,
    /// When the broker's answer arrived.
    at: Option<Instant>,
}

impl Preview {
    fn new(now: Instant) -> Self {
        Self {
            seen: None,
            changed_at: now,
            pending: None,
            answer: None,
            failure: None,
            answered_for: None,
            at: None,
        }
    }

    fn forget(&mut self) {
        self.pending = None;
        self.answer = None;
        self.failure = None;
        self.answered_for = None;
        self.at = None;
    }

    /// Asks again at once.
    fn ask_now(&mut self, now: Instant) {
        self.changed_at = now.checked_sub(PREVIEW_DEBOUNCE).unwrap_or(now);
    }

    fn age(&self, now: Instant) -> Option<Duration> {
        self.at.map(|at| now.saturating_duration_since(at))
    }

    fn is_old(&self, now: Instant) -> bool {
        self.age(now).is_some_and(|age| age > PREVIEW_FRESH)
    }
}

/// What this frame worked out about what is typed.
struct Computed {
    change: Result<OrderChange, NotAChange>,
    issues: Vec<EditIssue>,
    key: Option<Asked>,
    gate: PreviewGate,
    can_review: bool,
}

impl Default for Computed {
    fn default() -> Self {
        Self {
            change: Err(NotAChange::Unchanged),
            issues: Vec::new(),
            key: None,
            gate: PreviewGate::Waiting,
            can_review: false,
        }
    }
}

pub struct EditSession {
    order: LiveOrder,
    preview: Preview,
    stage: Stage,
    note: Option<EditNote>,
    computed: Computed,
}

impl EditSession {
    /// Starts editing `order`.
    pub fn new(order: LiveOrder, now: Instant) -> Self {
        Self {
            order,
            preview: Preview::new(now),
            stage: Stage::Editing,
            note: None,
            computed: Computed::default(),
        }
    }

    /// The order as the broker last reported it.
    pub fn order(&self) -> &LiveOrder {
        &self.order
    }

    pub fn is_sending(&self) -> bool {
        matches!(self.stage, Stage::Sending(_))
    }

    pub fn is_confirming(&self) -> bool {
        matches!(self.stage, Stage::Confirming(..))
    }

    /// What to draw for the last frame.
    pub fn state(&self) -> EditState<'_> {
        EditState {
            order: &self.order,
            review: match &self.stage {
                Stage::Confirming(change, preview) => Some(Review { change, preview }),
                _ => None,
            },
            change: &self.computed.change,
            issues: &self.computed.issues,
            preview: self.preview.answer.as_ref(),
            gate: &self.computed.gate,
            asking: self.computed.key.is_some() && self.computed.gate == PreviewGate::Waiting,
            note: self.note.as_ref(),
            can_review: self.computed.can_review,
            sending: self.is_sending(),
        }
    }

    /// Whether the broker's accepting answer for what is typed has arrived.
    pub fn is_ready(&self) -> bool {
        self.preview.answered_for.is_some()
            && self.preview.answered_for == self.preview.seen
            && self
                .preview
                .answer
                .as_ref()
                .is_some_and(EditPreview::is_acceptable)
    }

    /// Whether the preview of what is typed failed.
    pub fn preview_failed(&self) -> bool {
        self.preview.failure.is_some()
    }

    /// Reconciles the session with the world for one frame: adopts the order as it is now, works
    /// out what is typed, keeps the broker's preview of it current and settles an amend that was
    /// sent.
    pub fn update(&mut self, frame: &EditFrame, entry: &Entry, source: &dyn EditSource) -> Step {
        let now = frame.now;
        if frame
            .live
            .is_none_or(|order| order.status != OrderStatus::Open)
            && !self.is_sending()
        {
            return Step::Closed(Closing::NotOpen);
        }
        let Some(account) = frame.account else {
            return Step::Closed(Closing::NotConnected);
        };
        // Every figure below (what changed, the filled size, the old values in the question) is the
        // order as it is now, not as it was when the session opened: a fill or an edit made
        // elsewhere meanwhile must show, and must not leave a review standing that describes the
        // old order.
        if let Some(order) = frame.live {
            self.order = order.clone();
        }
        let fingerprint = OrderFingerprint::of(account, &self.order);
        let change = self.typed_change(entry);
        let issues = match (&change, frame.spec) {
            (Ok(change), Some(spec)) => edit_issues(&self.order, change, spec),
            _ => Vec::new(),
        };
        let key = match &change {
            Ok(change) if issues.is_empty() => Some(Asked {
                change: *change,
                order: fingerprint.clone(),
                generation: frame.generation,
            }),
            _ => None,
        };

        self.sync_preview(key.as_ref(), now, source);
        if let Some(step) = self.settle_amend() {
            return step;
        }
        self.drop_stale_review(&fingerprint, now);

        let answer = match (&key, &self.preview) {
            (Some(key), preview) if preview.answered_for.as_ref() == Some(key) => {
                match (&preview.answer, &preview.failure) {
                    (Some(answer), _) if answer.is_acceptable() => PreviewAnswer::Accepted,
                    (Some(_), _) => PreviewAnswer::Problems,
                    (None, Some(error)) => PreviewAnswer::Failed(error.clone()),
                    (None, None) => PreviewAnswer::Waiting,
                }
            }
            _ => PreviewAnswer::Waiting,
        };
        let gate = preview_gate(answer, self.preview.age(now));
        let can_review = key.is_some() && gate.allows_review();
        self.computed = Computed {
            change,
            issues,
            key,
            gate,
            can_review,
        };
        Step::Open
    }

    /// What the user pressed.
    pub fn handle(&mut self, event: EditEvent, now: Instant, source: &dyn EditSource) -> Step {
        match event {
            EditEvent::Review => {
                if self.computed.can_review
                    && let Ok(change) = self.computed.change
                    && let Some(preview) = self.preview.answer.clone()
                    && matches!(self.stage, Stage::Editing)
                {
                    self.stage = Stage::Confirming(change, Box::new(preview));
                }
            }
            EditEvent::Back => {
                if self.is_confirming() {
                    self.stage = Stage::Editing;
                }
            }
            EditEvent::Confirm => {
                if let Stage::Confirming(change, preview) = &self.stage {
                    let change = *change;
                    let base = preview.base.clone();
                    self.note = None;
                    match base {
                        // Sent against the order that was reviewed, which the runtime's worker
                        // checks again before anything leaves.
                        Some(base) => {
                            self.stage =
                                Stage::Sending(source.amend(self.order.id.clone(), change, base));
                        }
                        None => {
                            self.note = Some(EditNote::NotChecked);
                            self.stage = Stage::Editing;
                        }
                    }
                }
            }
            EditEvent::TryAgain => {
                self.preview.failure = None;
                self.preview.answered_for = None;
                self.preview.at = None;
                self.preview.ask_now(now);
            }
            EditEvent::Cancel => {
                if !self.is_sending() {
                    return Step::Closed(Closing::Dismissed);
                }
            }
        }
        Step::Open
    }

    fn typed_change(&self, entry: &Entry) -> Result<OrderChange, NotAChange> {
        let order = &self.order;
        let price = entry
            .price
            .map_err(|NotANumber| NotAChange::PriceNotANumber)?;
        let size = entry
            .size
            .map_err(|NotANumber| NotAChange::SizeNotANumber)?;
        let change = OrderChange {
            price: price
                .filter(|p| Some(*p) != order.limit_price && order.order_type == OrderType::Limit),
            stop_price: price.filter(|p| {
                Some(*p) != order.stop_price && order.order_type == OrderType::StopLimit
            }),
            size: size.filter(|s| matches!(order.size, Some(OrderSize::Base(b)) if b != *s)),
        };
        if change.is_empty() {
            Err(NotAChange::Unchanged)
        } else {
            Ok(change)
        }
    }

    /// Keeps the broker's preview current: forgets it when what it describes changed, asks again
    /// when it grew old, takes an answer that arrived and asks once typing has paused.
    fn sync_preview(&mut self, key: Option<&Asked>, now: Instant, source: &dyn EditSource) {
        let editing = matches!(self.stage, Stage::Editing);
        let preview = &mut self.preview;
        if key != preview.seen.as_ref() {
            preview.seen = key.cloned();
            preview.changed_at = now;
            preview.forget();
        }
        // An answer older than a minute is a statement about an older market: it is asked for again.
        if preview.is_old(now) && editing {
            preview.answer = None;
            preview.answered_for = None;
            preview.at = None;
            preview.ask_now(now);
        }
        if let Some((asked, pending)) = &mut preview.pending
            && let Some(answer) = pending.poll()
        {
            let asked = asked.clone();
            preview.pending = None;
            if Some(&asked) == preview.seen.as_ref() {
                match answer {
                    Ok(answer) => {
                        preview.answer = Some(answer);
                        preview.at = Some(now);
                    }
                    Err(error) => {
                        preview.failure = Some(error.to_string());
                        preview.at = None;
                    }
                }
                preview.answered_for = Some(asked);
            }
        }
        if let Some(key) = key
            && preview.answered_for.as_ref() != Some(key)
            && preview.pending.is_none()
            && now.saturating_duration_since(preview.changed_at) >= PREVIEW_DEBOUNCE
            && editing
        {
            preview.pending = Some((
                key.clone(),
                source.preview_edit(self.order.id.clone(), key.change),
            ));
        }
    }

    /// Takes the answer of an amend that was sent: done, or back to editing with the reason.
    fn settle_amend(&mut self) -> Option<Step> {
        let Stage::Sending(pending) = &mut self.stage else {
            return None;
        };
        match pending.poll()? {
            Ok(_) => Some(Step::Closed(Closing::Sent)),
            Err(error) if error.kind == ErrorKind::OutcomeUnknown => {
                Some(Step::Closed(Closing::OutcomeUnknown(error)))
            }
            Err(error) => {
                self.note = Some(EditNote::AmendFailed(error));
                self.stage = Stage::Editing;
                None
            }
        }
    }

    /// A review stands only while the order is the one it was worked out for, and the answer is
    /// recent: otherwise the session goes back to editing, and asks again.
    fn drop_stale_review(&mut self, fingerprint: &OrderFingerprint, now: Instant) {
        let Stage::Confirming(_, preview) = &self.stage else {
            return;
        };
        let same_order = preview.base.as_ref() == Some(fingerprint);
        let recent = self
            .preview
            .age(now)
            .is_some_and(|age| age <= PREVIEW_FRESH);
        if !same_order || !recent {
            self.stage = Stage::Editing;
            self.note = Some(if same_order {
                EditNote::PreviewOutOfDate
            } else {
                EditNote::OrderChanged
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::model::edit::tests::{order, spec};
    use crate::model::orders::Problem;
    use crate::model::test_support::d;

    type Answer<T> = Option<Result<T, BrokerError>>;

    fn accepted(order: &LiveOrder, account: &AccountId) -> EditPreview {
        EditPreview {
            order_total: Some(d("99")),
            fees: Some(d("0.5")),
            base: Some(OrderFingerprint::of(account, order)),
            ..EditPreview::default()
        }
    }

    fn failure(text: &str, kind: ErrorKind) -> BrokerError {
        BrokerError::new(kind, text)
    }

    /// A source that answers at once with what the test set (or never, when nothing is set), and
    /// remembers what it was asked.
    #[derive(Default)]
    struct Source {
        preview: RefCell<Answer<EditPreview>>,
        amend: RefCell<Answer<EditOutcome>>,
        previews: RefCell<Vec<OrderChange>>,
        amends: RefCell<Vec<(OrderChange, OrderFingerprint)>>,
    }

    fn answer<T: Clone>(slot: &RefCell<Answer<T>>) -> Pending<T> {
        match slot.borrow().clone() {
            Some(answer) => Pending::ready(answer),
            None => {
                // Never answered: keep the sender alive so the request stays pending.
                let (reply, receiver) = std::sync::mpsc::channel::<crate::error::Result<T>>();
                std::mem::forget(reply);
                Pending::from_receiver(receiver)
            }
        }
    }

    impl EditSource for Source {
        fn preview_edit(&self, _: OrderId, change: OrderChange) -> Pending<EditPreview> {
            self.previews.borrow_mut().push(change);
            answer(&self.preview)
        }

        fn amend(
            &self,
            _: OrderId,
            change: OrderChange,
            base: OrderFingerprint,
        ) -> Pending<EditOutcome> {
            self.amends.borrow_mut().push((change, base));
            answer(&self.amend)
        }
    }

    struct Rig {
        session: EditSession,
        source: Source,
        now: Instant,
        live: Option<LiveOrder>,
        account: Option<AccountId>,
        generation: u64,
        spec: Option<ProductSpec>,
        entry: Entry,
    }

    impl Rig {
        fn new() -> Self {
            let now = Instant::now();
            let rig = Self {
                session: EditSession::new(order(), now),
                source: Source::default(),
                now,
                live: Some(order()),
                account: Some(AccountId::new("acct")),
                generation: 1,
                spec: Some(spec()),
                entry: Entry {
                    price: Ok(Some(d("101"))),
                    size: Ok(Some(d("1"))),
                },
            };
            rig.source
                .preview
                .replace(Some(Ok(accepted(&order(), rig.account.as_ref().unwrap()))));
            rig
        }

        fn update(&mut self) -> Step {
            let frame = EditFrame {
                now: self.now,
                live: self.live.as_ref(),
                account: self.account.as_ref(),
                generation: self.generation,
                spec: self.spec.as_ref(),
            };
            self.session.update(&frame, &self.entry, &self.source)
        }

        fn press(&mut self, event: EditEvent) -> Step {
            self.session.handle(event, self.now, &self.source)
        }

        fn wait(&mut self, by: Duration) {
            self.now += by;
        }

        /// Typed, settled, asked and answered: ready to review.
        fn ready(&mut self) {
            self.update();
            self.wait(PREVIEW_DEBOUNCE);
            self.update();
            self.update();
            assert!(self.session.is_ready(), "the preview should have arrived");
        }

        fn confirming(&mut self) {
            self.ready();
            self.press(EditEvent::Review);
            assert!(self.session.is_confirming());
        }

        fn asked(&self) -> usize {
            self.source.previews.borrow().len()
        }
    }

    fn closed_with(step: Step) -> Closing {
        match step {
            Step::Closed(closing) => closing,
            Step::Open => panic!("the session should have closed"),
        }
    }

    #[test]
    fn nothing_is_asked_until_typing_has_paused() {
        let mut rig = Rig::new();
        rig.update();
        assert_eq!(rig.asked(), 0);
        rig.wait(PREVIEW_DEBOUNCE - Duration::from_millis(1));
        rig.update();
        assert_eq!(rig.asked(), 0, "still within the pause");
        rig.wait(Duration::from_millis(1));
        rig.update();
        assert_eq!(rig.asked(), 1);
        rig.update();
        assert_eq!(rig.asked(), 1, "asked once, not every frame");
    }

    #[test]
    fn review_is_enabled_only_on_the_brokers_fresh_accepting_answer() {
        let mut rig = Rig::new();
        rig.update();
        assert!(!rig.session.state().can_review, "no answer yet");
        rig.press(EditEvent::Review);
        assert!(
            !rig.session.is_confirming(),
            "a press before the answer does nothing"
        );
        rig.ready();
        assert!(rig.session.state().can_review);
        // The answer ages out: it is asked for again and review is off meanwhile.
        rig.source.preview.replace(None);
        rig.wait(PREVIEW_FRESH + Duration::from_secs(1));
        rig.update();
        assert!(!rig.session.state().can_review);
        rig.update();
        assert_eq!(rig.asked(), 2, "asked again");
    }

    #[test]
    fn a_preview_with_problems_or_a_failure_blocks_review_and_try_again_asks_at_once() {
        let mut rig = Rig::new();
        let account = rig.account.clone().unwrap();
        let mut refused = accepted(&order(), &account);
        refused.problems = vec![Problem {
            code: None,
            text: "Insufficient funds".to_owned(),
        }];
        rig.source.preview.replace(Some(Ok(refused)));
        rig.update();
        rig.wait(PREVIEW_DEBOUNCE);
        rig.update();
        rig.update();
        assert!(!rig.session.state().can_review);
        assert_eq!(*rig.session.state().gate, PreviewGate::Blocked);
        assert_eq!(
            rig.session.state().preview.unwrap().problems[0].text,
            "Insufficient funds"
        );

        rig.source.preview.replace(Some(Err(failure(
            "The connection dropped",
            ErrorKind::Transient,
        ))));
        rig.press(EditEvent::TryAgain);
        rig.update();
        rig.update();
        assert!(rig.session.preview_failed());
        assert!(!rig.session.state().can_review);
        let before = rig.asked();
        rig.source
            .preview
            .replace(Some(Ok(accepted(&order(), &account))));
        rig.press(EditEvent::TryAgain);
        rig.update();
        assert_eq!(
            rig.asked(),
            before + 1,
            "asked again at once, not after the pause"
        );
        rig.update();
        assert!(rig.session.state().can_review);
    }

    #[test]
    fn what_is_typed_is_checked_against_the_markets_rules_before_the_broker_is_asked() {
        let mut rig = Rig::new();
        rig.entry.price = Ok(Some(d("100.005")));
        rig.update();
        assert!(matches!(
            rig.session.state().issues,
            [EditIssue::PriceOffIncrement { .. }]
        ));
        rig.wait(PREVIEW_DEBOUNCE);
        rig.update();
        assert_eq!(rig.asked(), 0, "an invalid change is never previewed");

        rig.entry.size = Ok(Some(d("0.0005")));
        rig.entry.price = Ok(Some(d("101")));
        rig.update();
        let issues = rig.session.state().issues.to_vec();
        assert!(issues.contains(&EditIssue::SizeBelowMinimum { minimum: d("0.01") }));
        assert!(issues.contains(&EditIssue::SizeOffIncrement {
            increment: d("0.001")
        }));
    }

    #[test]
    fn the_size_cannot_go_below_what_has_filled() {
        let mut rig = Rig::new();
        let mut partly = order();
        partly.filled_size = d("0.6");
        rig.live = Some(partly);
        rig.entry.size = Ok(Some(d("0.5")));
        rig.update();
        assert_eq!(rig.session.state().issues, [EditIssue::SizeBelowFilled]);
    }

    #[test]
    fn text_that_is_not_a_number_or_not_a_change_says_so() {
        let mut rig = Rig::new();
        rig.entry.price = Err(NotANumber);
        rig.update();
        assert_eq!(
            *rig.session.state().change,
            Err(NotAChange::PriceNotANumber)
        );
        rig.entry.price = Ok(Some(d("100")));
        rig.entry.size = Err(NotANumber);
        rig.update();
        assert_eq!(*rig.session.state().change, Err(NotAChange::SizeNotANumber));
        rig.entry.size = Ok(Some(d("1")));
        rig.update();
        assert_eq!(*rig.session.state().change, Err(NotAChange::Unchanged));
        assert!(!rig.session.state().can_review);
    }

    #[test]
    fn a_review_carries_the_change_and_the_brokers_figures() {
        let mut rig = Rig::new();
        rig.confirming();
        let state = rig.session.state();
        let review = state.review.expect("reviewing");
        assert_eq!(review.change.price, Some(d("101")));
        assert_eq!(review.preview.order_total, Some(d("99")));
        assert_eq!(review.preview.fees, Some(d("0.5")));
    }

    #[test]
    fn confirming_sends_the_change_against_the_order_that_was_reviewed() {
        let mut rig = Rig::new();
        rig.confirming();
        assert!(
            rig.source.amends.borrow().is_empty(),
            "nothing is sent before approval"
        );
        rig.press(EditEvent::Confirm);
        let amends = rig.source.amends.borrow();
        assert_eq!(amends.len(), 1);
        assert_eq!(amends[0].0.price, Some(d("101")));
        assert_eq!(
            amends[0].1,
            OrderFingerprint::of(rig.account.as_ref().unwrap(), &order())
        );
        assert!(rig.session.is_sending());
    }

    #[test]
    fn a_change_the_broker_did_not_check_is_never_sent() {
        let mut rig = Rig::new();
        let mut unchecked = accepted(&order(), rig.account.as_ref().unwrap());
        unchecked.base = None;
        rig.source.preview.replace(Some(Ok(unchecked)));
        rig.confirming();
        rig.press(EditEvent::Confirm);
        assert!(rig.source.amends.borrow().is_empty());
        assert_eq!(rig.session.state().note, Some(&EditNote::NotChecked));
        assert!(!rig.session.is_confirming());
    }

    #[test]
    fn a_fill_or_an_edit_elsewhere_drops_the_review_and_sends_nothing() {
        let mut rig = Rig::new();
        rig.confirming();
        let mut filled = order();
        filled.filled_size = d("0.2");
        rig.live = Some(filled);
        assert!(matches!(rig.update(), Step::Open));
        assert!(!rig.session.is_confirming());
        assert_eq!(rig.session.state().note, Some(&EditNote::OrderChanged));
        assert_eq!(
            rig.session.order().filled_size,
            d("0.2"),
            "the session shows the order as it is now"
        );
        assert!(rig.source.amends.borrow().is_empty());
        // And it cannot be confirmed from the stale review.
        rig.press(EditEvent::Confirm);
        assert!(rig.source.amends.borrow().is_empty());
    }

    #[test]
    fn another_connection_or_account_drops_the_review() {
        let mut rig = Rig::new();
        rig.confirming();
        rig.generation = 2;
        rig.update();
        assert!(
            !rig.session.is_confirming(),
            "a preview is about one connection"
        );
        assert!(!rig.session.state().can_review);

        let mut rig = Rig::new();
        rig.confirming();
        rig.account = Some(AccountId::new("another"));
        rig.update();
        assert!(!rig.session.is_confirming(), "and about one account");
    }

    #[test]
    fn a_review_whose_answer_has_gone_stale_goes_back_to_editing() {
        let mut rig = Rig::new();
        rig.confirming();
        rig.wait(PREVIEW_FRESH + Duration::from_secs(1));
        rig.update();
        assert!(!rig.session.is_confirming());
        assert_eq!(rig.session.state().note, Some(&EditNote::PreviewOutOfDate));
    }

    #[test]
    fn the_session_closes_when_the_order_is_gone_or_the_account_is() {
        let mut rig = Rig::new();
        rig.live = None;
        assert_eq!(closed_with(rig.update()), Closing::NotOpen);

        let mut rig = Rig::new();
        let mut done = order();
        done.status = OrderStatus::Filled;
        rig.live = Some(done);
        assert_eq!(closed_with(rig.update()), Closing::NotOpen);

        let mut rig = Rig::new();
        rig.account = None;
        assert_eq!(closed_with(rig.update()), Closing::NotConnected);
    }

    #[test]
    fn an_amend_in_flight_survives_the_order_leaving_the_book() {
        let mut rig = Rig::new();
        rig.confirming();
        rig.press(EditEvent::Confirm);
        // The broker applied it: the order is no longer the open one we sent against.
        rig.live = None;
        assert!(
            matches!(rig.update(), Step::Open),
            "the answer is still awaited"
        );

        let mut sent = Rig::new();
        sent.source.amend.replace(Some(Ok(EditOutcome::Native)));
        sent.confirming();
        sent.press(EditEvent::Confirm);
        assert_eq!(closed_with(sent.update()), Closing::Sent);
    }

    #[test]
    fn a_failed_amend_returns_to_editing_with_the_reason_and_an_unknown_one_is_read_again() {
        let mut rig = Rig::new();
        rig.source.amend.replace(Some(Err(failure(
            "The price is too far from the market.",
            ErrorKind::Invalid,
        ))));
        rig.confirming();
        rig.press(EditEvent::Confirm);
        assert!(matches!(rig.update(), Step::Open));
        assert!(!rig.session.is_sending());
        assert!(matches!(
            rig.session.state().note,
            Some(EditNote::AmendFailed(error)) if error.kind == ErrorKind::Invalid
        ));

        let mut rig = Rig::new();
        rig.source.amend.replace(Some(Err(failure(
            "The connection dropped.",
            ErrorKind::OutcomeUnknown,
        ))));
        rig.confirming();
        rig.press(EditEvent::Confirm);
        assert!(matches!(
            closed_with(rig.update()),
            Closing::OutcomeUnknown(error) if error.kind == ErrorKind::OutcomeUnknown
        ));
    }

    #[test]
    fn back_and_cancel_leave_without_sending_and_cancel_waits_for_a_send_in_flight() {
        let mut rig = Rig::new();
        rig.confirming();
        rig.press(EditEvent::Back);
        assert!(!rig.session.is_confirming());
        assert!(rig.source.amends.borrow().is_empty());
        assert_eq!(
            closed_with(rig.press(EditEvent::Cancel)),
            Closing::Dismissed
        );

        let mut rig = Rig::new();
        rig.confirming();
        rig.press(EditEvent::Confirm);
        assert!(
            matches!(rig.press(EditEvent::Cancel), Step::Open),
            "an amend in flight cannot be abandoned"
        );
    }

    #[test]
    fn a_stop_limit_edits_its_stop_price() {
        let mut stop = order();
        stop.order_type = OrderType::StopLimit;
        stop.stop_price = Some(d("95"));
        let now = Instant::now();
        let mut session = EditSession::new(stop.clone(), now);
        let source = Source::default();
        let account = AccountId::new("acct");
        let spec = spec();
        let frame = EditFrame {
            now,
            live: Some(&stop),
            account: Some(&account),
            generation: 1,
            spec: Some(&spec),
        };
        let entry = Entry {
            price: Ok(Some(d("96"))),
            size: Ok(Some(d("1"))),
        };
        session.update(&frame, &entry, &source);
        let change = (*session.state().change).unwrap();
        assert_eq!(change.stop_price, Some(d("96")));
        assert_eq!(change.price, None);
    }
}
