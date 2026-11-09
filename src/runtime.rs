//! One broker, kept in sync, on one worker thread.
//!
//! [`BrokerRuntime`] owns the connection, the account stream and the [`Book`]. An interface never
//! calls the network: it sends a request and gets a [`Pending`] it polls, and it reads the latest
//! [`BrokerSnapshot`] (an `Arc`, replaced whole, so reading never blocks on a request).
//!
//! The broker is the source of truth. State is built from a REST read plus the live stream, and
//! read again whenever the two may have diverged: a reconnect, a missed message, the machine
//! waking from sleep, an order whose submission outcome is unknown, and on a slow timer as a
//! backstop. Stream messages that arrive while a read is running are applied after it, in order,
//! so nothing falls in the gap between the snapshot and the stream.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::adapter::{Broker, Connection, Env, Session, StreamHandle};
use crate::error::{BrokerError, ErrorKind, Result};
use crate::model::events::{EventSink, StreamEvent};
use crate::model::orders::{
    CancelOutcome, CancelResult, EditOutcome, EditPreview, FeeSummary, FillFilter, HistoryFilter,
    OrderChange, OrderPreview, Page, PlacedOrder,
};
use crate::model::pending::{PendingJournal, PendingSubmission, new_client_order_id};
use crate::model::reducer::{Book, Notice, OrderView, Source};
use crate::model::types::{
    AccountId, Balance, BrokerId, ClientOrderId, LiveFill, LiveOrder, OrderDraft, OrderId,
    ProductId, ProductSpec, Quote,
};
use crate::model::validate::validate_draft;
use crate::secret::Credentials;
use crate::submit;

/// Asked to redraw. Called from the worker thread; must not block.
pub trait Waker: Send + Sync {
    fn wake(&self);
}

/// Receives each [`Notice`] exactly once (never for the initial listing, never twice for a replayed
/// message).
pub trait NoticeSink: Send + Sync {
    fn notice(&self, notice: Notice);
}

#[derive(Clone)]
pub struct RuntimeConfig {
    pub env: Env,
    pub journal: Arc<dyn PendingJournal>,
    pub waker: Arc<dyn Waker>,
    pub notices: Arc<dyn NoticeSink>,
    /// State is read again this often while everything looks healthy.
    pub safety_poll: Duration,
    /// Unresolved submissions are looked up this often.
    pub reconcile_every: Duration,
    /// A gap this long between two ticks means the machine slept.
    pub wake_gap: Duration,
    /// How often the worker wakes with nothing to do.
    pub tick: Duration,
    /// The least time between two full reads, so a flapping stream cannot hammer the API.
    pub min_sync_gap: Duration,
}

impl RuntimeConfig {
    pub fn new(
        env: Env,
        journal: Arc<dyn PendingJournal>,
        waker: Arc<dyn Waker>,
        notices: Arc<dyn NoticeSink>,
    ) -> Self {
        Self {
            env,
            journal,
            waker,
            notices,
            safety_poll: Duration::from_secs(60),
            reconcile_every: Duration::from_secs(15),
            wake_gap: Duration::from_secs(5),
            tick: Duration::from_secs(1),
            min_sync_gap: Duration::from_secs(5),
        }
    }

    /// Short intervals, for tests.
    #[must_use]
    pub fn fast(mut self) -> Self {
        self.safety_poll = Duration::from_millis(400);
        self.reconcile_every = Duration::from_millis(100);
        self.wake_gap = Duration::from_millis(300);
        self.tick = Duration::from_millis(15);
        self.min_sync_gap = Duration::from_millis(40);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Status {
    /// Opening the connection.
    Connecting,
    /// Reading balances and orders.
    Syncing,
    /// In sync, and the stream is up.
    Live,
    /// The stream or the network is down; state shown may be behind. Trying again.
    Reconnecting {
        since_ms: i64,
        reason: String,
    },
    /// The broker refused the credentials, or they lack permission. Not retried: the user must act.
    Failed(BrokerError),
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingState {
    /// Being sent now.
    Sending,
    /// Sent, and we do not know whether the broker took it.
    Unknown,
}

/// An order that has been asked for and is not yet confirmed either way.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingView {
    pub client_id: ClientOrderId,
    pub draft: OrderDraft,
    pub created_ms: i64,
    pub state: PendingState,
}

/// Everything an interface needs to draw, as of one moment.
#[derive(Clone, Debug)]
pub struct BrokerSnapshot {
    /// Rises whenever anything here changes.
    pub revision: u64,
    pub broker: BrokerId,
    pub status: Status,
    pub session: Option<Session>,
    pub balances: Vec<Balance>,
    pub balances_ms: Option<i64>,
    pub orders: Vec<OrderView>,
    pub fills: Vec<LiveFill>,
    pub pending: Vec<PendingView>,
    pub products: Arc<Vec<ProductSpec>>,
    pub fees: Option<FeeSummary>,
    pub last_sync_ms: Option<i64>,
    pub last_error: Option<String>,
}

impl BrokerSnapshot {
    fn empty(broker: BrokerId) -> Self {
        Self {
            revision: 0,
            broker,
            status: Status::Connecting,
            session: None,
            balances: Vec::new(),
            balances_ms: None,
            orders: Vec::new(),
            fills: Vec::new(),
            pending: Vec::new(),
            products: Arc::new(Vec::new()),
            fees: None,
            last_sync_ms: None,
            last_error: None,
        }
    }

    pub fn product(&self, id: &ProductId) -> Option<&ProductSpec> {
        self.products.iter().find(|p| &p.id == id)
    }

    pub fn working_orders(&self) -> impl Iterator<Item = &OrderView> {
        self.orders.iter().filter(|v| v.order.status.is_working())
    }

    /// Whether a new order may be sent now.
    pub fn is_live(&self) -> bool {
        self.status == Status::Live
    }
}

/// A request in flight. Poll it from the interface's frame loop.
pub struct Pending<T>(Receiver<Result<T>>);

impl<T> Pending<T> {
    /// `None` while the request is still running.
    pub fn poll(&self) -> Option<Result<T>> {
        match self.0.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(BrokerError::new(
                ErrorKind::Transient,
                "The connection to the broker was closed.",
            ))),
        }
    }

    /// Waits for the answer. For tests and for code already on a worker thread.
    pub fn wait(&self) -> Result<T> {
        self.0.recv().unwrap_or_else(|_| {
            Err(BrokerError::new(
                ErrorKind::Transient,
                "The connection to the broker was closed.",
            ))
        })
    }
}

type Reply<T> = Sender<Result<T>>;

enum Command {
    Submit(OrderDraft, Reply<PlacedOrder>),
    Retry(ClientOrderId, Reply<PlacedOrder>),
    Preview(OrderDraft, Reply<OrderPreview>),
    Cancel(Vec<OrderId>, Reply<Vec<CancelResult>>),
    PreviewEdit(OrderId, OrderChange, Reply<EditPreview>),
    Amend(OrderId, OrderChange, Reply<EditOutcome>),
    History(HistoryFilter, Option<String>, Reply<Page<LiveOrder>>),
    Fills(FillFilter, Option<String>, Reply<Page<LiveFill>>),
    Quote(ProductId, Reply<Quote>),
    Product(ProductId, Reply<ProductSpec>),
    Refresh,
    Stop,
}

enum Message {
    Command(Command),
    Stream(StreamEvent),
}

struct ForwardSink(Sender<Message>);

impl EventSink for ForwardSink {
    fn event(&self, event: StreamEvent) {
        let _ = self.0.send(Message::Stream(event));
    }
}

struct Shared {
    snapshot: RwLock<Arc<BrokerSnapshot>>,
    revision: AtomicU64,
}

pub struct BrokerRuntime {
    shared: Arc<Shared>,
    sender: Sender<Message>,
    thread: Option<JoinHandle<()>>,
}

impl BrokerRuntime {
    /// Starts connecting. Returns at once; watch [`BrokerRuntime::snapshot`] for the result.
    pub fn start(
        broker: &'static dyn Broker,
        credentials: Credentials,
        config: RuntimeConfig,
    ) -> Self {
        let id = broker.descriptor().id;
        let shared = Arc::new(Shared {
            snapshot: RwLock::new(Arc::new(BrokerSnapshot::empty(id))),
            revision: AtomicU64::new(0),
        });
        let (sender, receiver) = mpsc::channel();
        let worker = Worker::new(
            broker,
            credentials,
            config,
            Arc::clone(&shared),
            sender.clone(),
        );
        let thread = std::thread::Builder::new()
            .name(format!("{id}-runtime"))
            .spawn(move || worker.run(&receiver))
            .expect("spawn the broker runtime");
        Self {
            shared,
            sender,
            thread: Some(thread),
        }
    }

    pub fn snapshot(&self) -> Arc<BrokerSnapshot> {
        Arc::clone(&self.shared.snapshot.read().expect("snapshot lock"))
    }

    /// A cheap check for "has anything changed since I last drew".
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    fn ask<T>(&self, command: impl FnOnce(Reply<T>) -> Command) -> Pending<T> {
        let (reply, receiver) = mpsc::channel();
        let _ = self.sender.send(Message::Command(command(reply)));
        Pending(receiver)
    }

    /// Validates, records and sends an order. See [`crate::submit`] for what the answer means; an
    /// `OutcomeUnknown` error means the order is shown as unconfirmed until it is looked up.
    pub fn submit(&self, draft: OrderDraft) -> Pending<PlacedOrder> {
        self.ask(|reply| Command::Submit(draft, reply))
    }

    /// Sends an unconfirmed submission again, under its original client id, after looking first.
    pub fn retry(&self, client_id: ClientOrderId) -> Pending<PlacedOrder> {
        self.ask(|reply| Command::Retry(client_id, reply))
    }

    pub fn preview(&self, draft: OrderDraft) -> Pending<OrderPreview> {
        self.ask(|reply| Command::Preview(draft, reply))
    }

    pub fn cancel(&self, orders: Vec<OrderId>) -> Pending<Vec<CancelResult>> {
        self.ask(|reply| Command::Cancel(orders, reply))
    }

    pub fn preview_edit(&self, order: OrderId, change: OrderChange) -> Pending<EditPreview> {
        self.ask(|reply| Command::PreviewEdit(order, change, reply))
    }

    pub fn amend(&self, order: OrderId, change: OrderChange) -> Pending<EditOutcome> {
        self.ask(|reply| Command::Amend(order, change, reply))
    }

    pub fn history(
        &self,
        filter: HistoryFilter,
        cursor: Option<String>,
    ) -> Pending<Page<LiveOrder>> {
        self.ask(|reply| Command::History(filter, cursor, reply))
    }

    pub fn fills(&self, filter: FillFilter, cursor: Option<String>) -> Pending<Page<LiveFill>> {
        self.ask(|reply| Command::Fills(filter, cursor, reply))
    }

    pub fn quote(&self, product: ProductId) -> Pending<Quote> {
        self.ask(|reply| Command::Quote(product, reply))
    }

    /// One market's rules, fresh from the broker (and remembered).
    pub fn product(&self, product: ProductId) -> Pending<ProductSpec> {
        self.ask(|reply| Command::Product(product, reply))
    }

    /// Reads everything again now.
    pub fn refresh(&self) {
        let _ = self.sender.send(Message::Command(Command::Refresh));
    }

    /// Stops the worker and its stream, and waits for them.
    pub fn stop(mut self) {
        let _ = self.sender.send(Message::Command(Command::Stop));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for BrokerRuntime {
    fn drop(&mut self) {
        // Ask it to stop; do not wait (a request may be in flight).
        let _ = self.sender.send(Message::Command(Command::Stop));
    }
}

// ---- The worker ----------------------------------------------------------------------------------

/// Orders to read again, with when.
struct Refetch {
    id: OrderId,
    due_ms: i64,
}

struct Worker {
    broker: &'static dyn Broker,
    credentials: Option<Credentials>,
    config: RuntimeConfig,
    shared: Arc<Shared>,
    sender: Sender<Message>,

    connection: Option<Arc<dyn Connection>>,
    stream: Option<StreamHandle>,
    book: Book,
    snapshot: BrokerSnapshot,

    catalogue: HashMap<ProductId, ProductSpec>,
    catalogue_ms: Option<i64>,
    pending: BTreeMap<String, PendingView>,
    refetch: Vec<Refetch>,

    needs_resync: bool,
    stream_was_down: bool,
    synced_once: bool,
    last_sync_ms: i64,
    last_reconcile_ms: i64,
    next_connect_ms: i64,
    connect_failures: u32,
    balances_due_ms: Option<i64>,
    fills_due_ms: Option<i64>,
    dirty: bool,
}

const CATALOGUE_TTL_MS: i64 = 10 * 60 * 1000;

impl Worker {
    fn new(
        broker: &'static dyn Broker,
        credentials: Credentials,
        config: RuntimeConfig,
        shared: Arc<Shared>,
        sender: Sender<Message>,
    ) -> Self {
        let now = config.env.clock.now_ms();
        let id = broker.descriptor().id;
        let mut pending = BTreeMap::new();
        if let Ok(entries) = config.journal.unresolved(id) {
            for entry in entries {
                pending.insert(
                    entry.client_id.0.clone(),
                    PendingView {
                        client_id: entry.client_id,
                        draft: entry.draft,
                        created_ms: entry.created_ms,
                        state: PendingState::Unknown,
                    },
                );
            }
        }
        Self {
            broker,
            credentials: Some(credentials),
            config,
            shared,
            sender,
            connection: None,
            stream: None,
            book: Book::new(),
            snapshot: BrokerSnapshot::empty(id),
            catalogue: HashMap::new(),
            catalogue_ms: None,
            pending,
            refetch: Vec::new(),
            needs_resync: false,
            stream_was_down: false,
            synced_once: false,
            last_sync_ms: 0,
            last_reconcile_ms: now,
            next_connect_ms: 0,
            connect_failures: 0,
            balances_due_ms: None,
            fills_due_ms: None,
            dirty: true,
        }
    }

    fn now(&self) -> i64 {
        self.config.env.clock.now_ms()
    }

    fn run(mut self, receiver: &Receiver<Message>) {
        self.publish();
        loop {
            let waiting_since = self.now();
            let received = receiver.recv_timeout(self.config.tick);
            self.check_wake(waiting_since);
            match received {
                Ok(Message::Command(Command::Stop)) => break,
                Ok(Message::Command(command)) => self.command(command),
                Ok(Message::Stream(event)) => self.stream_event(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.housekeeping();
            if self.dirty {
                self.publish();
            }
        }
        if let Some(mut stream) = self.stream.take() {
            stream.stop();
        }
        self.snapshot.status = Status::Stopped;
        self.publish();
    }

    fn publish(&mut self) {
        self.dirty = false;
        let revision = self.shared.revision.fetch_add(1, Ordering::AcqRel) + 1;
        self.snapshot.revision = revision;
        self.snapshot.orders = self.book.views();
        self.snapshot.fills = self.book.fills().to_vec();
        self.snapshot.pending = self.pending.values().cloned().collect();
        let snapshot = Arc::new(self.snapshot.clone());
        *self.shared.snapshot.write().expect("snapshot lock") = snapshot;
        self.config.waker.wake();
    }

    fn set_status(&mut self, status: Status) {
        if self.snapshot.status != status {
            self.snapshot.status = status;
            self.dirty = true;
        }
    }

    fn note_error(&mut self, error: &BrokerError) {
        self.snapshot.last_error = Some(error.to_string());
        self.dirty = true;
    }

    // ---- Housekeeping -----------------------------------------------------------------------

    /// Waiting for a message is bounded by one tick. If it took far longer, the machine slept (or
    /// the clock jumped): everything we know may be behind, and the socket is probably dead. Time
    /// spent *working* is never counted: only time spent waiting.
    fn check_wake(&mut self, waiting_since: i64) {
        let now = self.now();
        let gap = i64::try_from(self.config.wake_gap.as_millis()).unwrap_or(i64::MAX);
        if now.saturating_sub(waiting_since) > gap && self.connection.is_some() {
            self.needs_resync = true;
            self.stream_was_down = true;
            if let Some(stream) = &self.stream {
                stream.reconnect();
            }
            self.set_status(Status::Reconnecting {
                since_ms: now,
                reason: "The computer woke from sleep.".to_owned(),
            });
        }
    }

    fn housekeeping(&mut self) {
        let now = self.now();
        if self.connection.is_none() {
            if matches!(self.snapshot.status, Status::Failed(_)) || now < self.next_connect_ms {
                return;
            }
            self.connect();
            if self.connection.is_none() {
                return;
            }
        }

        let gap = i64::try_from(self.config.min_sync_gap.as_millis()).unwrap_or(i64::MAX);
        let poll = i64::try_from(self.config.safety_poll.as_millis()).unwrap_or(i64::MAX);
        if matches!(self.snapshot.status, Status::Live) && now - self.last_sync_ms >= poll {
            self.needs_resync = true;
        }
        if self.needs_resync && (!self.synced_once || now - self.last_sync_ms >= gap) {
            self.sync();
        }
        if matches!(self.snapshot.status, Status::Failed(_)) {
            return;
        }

        self.process_refetches(now);
        if self.balances_due_ms.is_some_and(|due| now >= due) {
            self.balances_due_ms = None;
            self.refresh_balances();
        }
        if self.fills_due_ms.is_some_and(|due| now >= due) {
            self.fills_due_ms = None;
            self.refresh_fills();
        }
        if self.book.expire_overlays(now) {
            self.dirty = true;
        }
        let reconcile = i64::try_from(self.config.reconcile_every.as_millis()).unwrap_or(i64::MAX);
        if !self.pending.is_empty() && now - self.last_reconcile_ms >= reconcile {
            self.last_reconcile_ms = now;
            self.reconcile_pending();
        }
    }

    fn connect(&mut self) {
        let Some(credentials) = self.credentials.clone() else {
            return;
        };
        self.set_status(Status::Connecting);
        match self.broker.connect(&credentials, &self.config.env) {
            Ok(connection) => {
                self.snapshot.session = Some(connection.session().clone());
                self.snapshot.last_error = None;
                let sink = Arc::new(ForwardSink(self.sender.clone()));
                match connection.open_stream(sink) {
                    Ok(stream) => self.stream = Some(stream),
                    Err(error) => self.note_error(&error),
                }
                self.connection = Some(Arc::from(connection));
                // The credentials are not needed again: the connection holds what it signs with.
                self.credentials = None;
                self.connect_failures = 0;
                self.needs_resync = true;
                self.dirty = true;
            }
            Err(error) => {
                self.note_error(&error);
                let permanent = matches!(
                    error.kind,
                    ErrorKind::Auth
                        | ErrorKind::Permission
                        | ErrorKind::Invalid
                        | ErrorKind::Unsupported
                );
                if permanent {
                    self.set_status(Status::Failed(error));
                } else {
                    self.connect_failures += 1;
                    let wait = 1_000_i64 << self.connect_failures.min(5);
                    self.next_connect_ms = self.now() + wait.min(30_000);
                    self.set_status(Status::Reconnecting {
                        since_ms: self.now(),
                        reason: error.to_string(),
                    });
                }
            }
        }
    }

    /// Reads balances, open orders and recent fills, and applies them. Stream messages that arrived
    /// meanwhile are waiting in the queue and apply after this, in order.
    fn sync(&mut self) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        self.needs_resync = false;
        if !self.synced_once {
            self.set_status(Status::Syncing);
        }
        let now = self.config.env.clock.now_ms();
        let result = (|| -> Result<_> {
            let balances = connection.balances()?;
            let open = connection.open_orders()?;
            let fills = connection.fills(
                &FillFilter {
                    limit: 100,
                    ..FillFilter::default()
                },
                None,
            )?;
            let products = if self
                .catalogue_ms
                .is_none_or(|at| now - at > CATALOGUE_TTL_MS)
            {
                Some(connection.products()?)
            } else {
                None
            };
            let fees = if self.snapshot.fees.is_none() {
                connection.fee_summary().ok()
            } else {
                None
            };
            Ok((balances, open, fills.items, products, fees))
        })();

        let (balances, open, fills, products, fees) = match result {
            Ok(read) => read,
            Err(error) => return self.sync_failed(&error),
        };
        self.snapshot.balances = balances;
        self.snapshot.balances_ms = Some(now);
        if let Some(products) = products {
            self.catalogue = products.iter().map(|p| (p.id.clone(), p.clone())).collect();
            self.catalogue_ms = Some(now);
            self.snapshot.products = Arc::new(products);
        }
        if fees.is_some() {
            self.snapshot.fees = fees;
        }
        let (notices, missing) = self.book.apply_open_listing(open);
        self.deliver(notices);
        for fill in fills.into_iter().rev() {
            self.book.apply_fill(fill);
        }
        // Orders we thought open that the listing lacks: ask how they ended.
        for id in missing {
            self.fetch_order(&id);
        }
        self.synced_once = true;
        self.last_sync_ms = now;
        self.snapshot.last_sync_ms = Some(now);
        self.snapshot.last_error = None;
        if !self.stream_was_down {
            self.set_status(Status::Live);
        } else {
            // Up again only when the stream says so (see `stream_event`).
            self.set_status(Status::Reconnecting {
                since_ms: now,
                reason: "Waiting for the live connection.".to_owned(),
            });
        }
        self.dirty = true;
    }

    fn sync_failed(&mut self, error: &BrokerError) {
        self.note_error(error);
        match error.kind {
            ErrorKind::Auth | ErrorKind::Permission => {
                // Revoked or narrowed while running: stop trading on stale state.
                if let Some(mut stream) = self.stream.take() {
                    stream.stop();
                }
                self.connection = None;
                self.set_status(Status::Failed(error.clone()));
            }
            ErrorKind::RateLimited { retry_after_ms } => {
                self.needs_resync = true;
                self.last_sync_ms =
                    self.now() + i64::try_from(retry_after_ms.unwrap_or(5_000)).unwrap_or(5_000);
                self.set_status(Status::Reconnecting {
                    since_ms: self.now(),
                    reason: error.to_string(),
                });
            }
            _ => {
                self.needs_resync = true;
                self.last_sync_ms = self.now();
                self.set_status(Status::Reconnecting {
                    since_ms: self.now(),
                    reason: error.to_string(),
                });
            }
        }
    }

    fn refresh_balances(&mut self) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        match connection.balances() {
            Ok(balances) => {
                self.snapshot.balances = balances;
                self.snapshot.balances_ms = Some(self.now());
                self.dirty = true;
            }
            Err(error) => self.note_error(&error),
        }
    }

    fn refresh_fills(&mut self) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        match connection.fills(
            &FillFilter {
                limit: 100,
                ..FillFilter::default()
            },
            None,
        ) {
            Ok(page) => {
                for fill in page.items.into_iter().rev() {
                    if self.book.apply_fill(fill) {
                        self.dirty = true;
                    }
                }
            }
            Err(error) => self.note_error(&error),
        }
    }

    fn deliver(&mut self, notices: Vec<Notice>) {
        for notice in notices {
            self.config.notices.notice(notice);
        }
    }

    fn apply_order(&mut self, order: LiveOrder, source: Source) {
        let applied = self.book.apply_order(order, source);
        if applied.changed {
            self.dirty = true;
        }
        self.deliver(applied.notices);
    }

    fn fetch_order(&mut self, id: &OrderId) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        match connection.order(id) {
            Ok(order) => self.apply_order(order, Source::Refetch),
            Err(error) => self.note_error(&error),
        }
    }

    fn schedule_refetch(&mut self, id: OrderId, after_ms: i64) {
        self.refetch.push(Refetch {
            id,
            due_ms: self.now() + after_ms,
        });
    }

    fn process_refetches(&mut self, now: i64) {
        if self.refetch.is_empty() {
            return;
        }
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.refetch)
            .into_iter()
            .partition(|r| r.due_ms <= now);
        self.refetch = later;
        for refetch in due {
            self.fetch_order(&refetch.id);
        }
    }

    fn reconcile_pending(&mut self) {
        let Some(connection) = self.connection.clone() else {
            return;
        };
        let id = self.broker.descriptor().id;
        let Ok(entries) = self.config.journal.unresolved(id) else {
            return;
        };
        let now = self.config.env.clock.now_ms();
        for entry in entries {
            match submit::reconcile(
                connection.as_ref(),
                self.config.journal.as_ref(),
                &entry,
                now,
            ) {
                Ok(submit::Reconciled::Placed(order)) => {
                    self.pending.remove(&entry.client_id.0);
                    self.apply_order(*order, Source::Refetch);
                    self.fills_due_ms = Some(now);
                    self.balances_due_ms = Some(now);
                    self.dirty = true;
                }
                Ok(submit::Reconciled::NotPlaced) => {
                    self.pending.remove(&entry.client_id.0);
                    self.dirty = true;
                }
                Ok(submit::Reconciled::NotListedYet) => {}
                Err(error) => self.note_error(&error),
            }
        }
    }

    // ---- Stream events ----------------------------------------------------------------------

    fn stream_event(&mut self, event: StreamEvent) {
        let now = self.now();
        match event {
            StreamEvent::Connected => {
                if self.stream_was_down {
                    // Whatever happened while it was down is not in the stream: read it.
                    self.stream_was_down = false;
                    self.needs_resync = true;
                }
                if self.synced_once && !self.needs_resync {
                    self.set_status(Status::Live);
                }
            }
            StreamEvent::Orders { orders, snapshot } => {
                let source = if snapshot {
                    Source::Snapshot
                } else {
                    Source::Stream
                };
                let mut moved = false;
                for order in orders {
                    let before = self.book.get(&order.id).map(|o| o.filled_size);
                    let finished = order.status.is_terminal();
                    moved |= finished || before.is_none_or(|b| order.filled_size > b);
                    self.apply_order(order, source);
                }
                if moved && !snapshot {
                    // Fills and balances change with these; the stream carries neither.
                    self.fills_due_ms.get_or_insert(now + 300);
                    self.balances_due_ms.get_or_insert(now + 300);
                }
            }
            StreamEvent::Gap => {
                self.stream_was_down = false;
                self.needs_resync = true;
            }
            StreamEvent::Disconnected { reason } => {
                self.stream_was_down = true;
                if self.synced_once {
                    self.set_status(Status::Reconnecting {
                        since_ms: now,
                        reason,
                    });
                }
            }
        }
    }

    // ---- Commands ---------------------------------------------------------------------------

    fn not_connected() -> BrokerError {
        BrokerError::new(
            ErrorKind::Transient,
            "Not connected to the broker. Wait for the connection to come back.",
        )
    }

    /// Publishes anything that changed, then answers: a caller that reads the snapshot after its
    /// reply must see the state the request left behind.
    fn answer<T>(&mut self, reply: &Reply<T>, result: Result<T>) {
        if self.dirty {
            self.publish();
        }
        let _ = reply.send(result);
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Submit(draft, reply) => {
                let result = self.submit(draft);
                self.answer(&reply, result);
            }
            Command::Retry(client_id, reply) => {
                let result = self.retry(&client_id);
                self.answer(&reply, result);
            }
            Command::Preview(draft, reply) => {
                let result = self.with_connection(|c| c.preview(&draft));
                self.answer(&reply, result);
            }
            Command::Cancel(ids, reply) => {
                let result = self.cancel(&ids);
                self.answer(&reply, result);
            }
            Command::PreviewEdit(id, change, reply) => {
                let result = match self.book.get(&id).cloned() {
                    Some(order) => self.with_connection(|c| c.preview_edit(&order, &change)),
                    None => Err(BrokerError::new(
                        ErrorKind::NotFound,
                        "That order is not open.",
                    )),
                };
                self.answer(&reply, result);
            }
            Command::Amend(id, change, reply) => {
                let result = self.amend(&id, &change);
                self.answer(&reply, result);
            }
            Command::History(filter, cursor, reply) => {
                let result = self.with_connection(|c| c.order_history(&filter, cursor.as_deref()));
                self.answer(&reply, result);
            }
            Command::Fills(filter, cursor, reply) => {
                let result = self.with_connection(|c| c.fills(&filter, cursor.as_deref()));
                self.answer(&reply, result);
            }
            Command::Quote(product, reply) => {
                let result = self.with_connection(|c| c.quote(&product));
                self.answer(&reply, result);
            }
            Command::Product(product, reply) => {
                let result = self.product(&product);
                self.answer(&reply, result);
            }
            Command::Refresh => self.needs_resync = true,
            Command::Stop => {}
        }
    }

    fn with_connection<T>(&mut self, call: impl FnOnce(&dyn Connection) -> Result<T>) -> Result<T> {
        let result = match self.connection.clone() {
            Some(connection) => call(connection.as_ref()),
            None => Err(Self::not_connected()),
        };
        if let Err(error) = &result
            && matches!(error.kind, ErrorKind::Auth | ErrorKind::Permission)
        {
            self.note_error(error);
        }
        result
    }

    /// A market's rules: remembered, and fetched again when old.
    fn product(&mut self, id: &ProductId) -> Result<ProductSpec> {
        let now = self.now();
        if let Some(spec) = self.catalogue.get(id)
            && !spec.is_stale(now)
        {
            return Ok(spec.clone());
        }
        let spec = self.with_connection(|c| c.product(id))?;
        self.catalogue.insert(id.clone(), spec.clone());
        let mut products: Vec<ProductSpec> = self.snapshot.products.as_ref().clone();
        match products.iter_mut().find(|p| p.id == spec.id) {
            Some(existing) => *existing = spec.clone(),
            None => products.push(spec.clone()),
        }
        self.snapshot.products = Arc::new(products);
        self.dirty = true;
        Ok(spec)
    }

    fn submit(&mut self, draft: OrderDraft) -> Result<PlacedOrder> {
        if !self.snapshot.is_live() {
            return Err(Self::not_connected());
        }
        let spec = self.product(&draft.product)?;
        let (caps, rules) = match self.connection.clone() {
            Some(connection) => (
                connection.capabilities(&spec),
                self.broker.descriptor().client_id,
            ),
            None => return Err(Self::not_connected()),
        };
        let now = self.now();
        if let Some(issue) = validate_draft(&spec, &caps, &draft, now).into_iter().next() {
            return Err(BrokerError::invalid(issue.to_string()));
        }
        let client_id = new_client_order_id(&rules).ok_or_else(|| {
            BrokerError::invalid("This broker's order ids are too short for the ids used here.")
        })?;
        self.pending.insert(
            client_id.0.clone(),
            PendingView {
                client_id: client_id.clone(),
                draft: draft.clone(),
                created_ms: now,
                state: PendingState::Sending,
            },
        );
        self.publish();
        let connection = self.connection.clone().ok_or_else(Self::not_connected)?;
        let result = submit::submit(
            connection.as_ref(),
            self.config.journal.as_ref(),
            &draft,
            &client_id,
            now,
        );
        match &result {
            Ok(placed) => {
                self.pending.remove(&client_id.0);
                // Show the order at once, as the broker has it, rather than waiting for the stream.
                self.fetch_order(&placed.order_id);
                if self.book.get(&placed.order_id).is_none() {
                    self.schedule_refetch(placed.order_id.clone(), 1_000);
                }
                self.fills_due_ms = Some(now + 500);
                self.balances_due_ms = Some(now + 500);
            }
            Err(error) if error.kind == ErrorKind::OutcomeUnknown => {
                if let Some(view) = self.pending.get_mut(&client_id.0) {
                    view.state = PendingState::Unknown;
                }
                self.last_reconcile_ms = 0;
            }
            Err(_) => {
                self.pending.remove(&client_id.0);
            }
        }
        self.dirty = true;
        result
    }

    fn retry(&mut self, client_id: &ClientOrderId) -> Result<PlacedOrder> {
        let id = self.broker.descriptor().id;
        let entry: PendingSubmission = self
            .config
            .journal
            .unresolved(id)
            .map_err(|e| BrokerError::invalid(e.to_string()))?
            .into_iter()
            .find(|entry| &entry.client_id == client_id)
            .ok_or_else(|| {
                BrokerError::new(ErrorKind::NotFound, "That order is no longer waiting.")
            })?;
        let rules = self.broker.descriptor().client_id;
        let now = self.now();
        let connection = self.connection.clone().ok_or_else(Self::not_connected)?;
        let result = submit::retry(
            connection.as_ref(),
            self.config.journal.as_ref(),
            &rules,
            &entry,
            now,
        );
        match &result {
            Ok(placed) => {
                self.pending.remove(&client_id.0);
                self.fetch_order(&placed.order_id);
                if self.book.get(&placed.order_id).is_none() {
                    self.schedule_refetch(placed.order_id.clone(), 1_000);
                }
            }
            Err(error) if error.kind == ErrorKind::OutcomeUnknown => {}
            Err(_) => {
                self.pending.remove(&client_id.0);
            }
        }
        self.dirty = true;
        result
    }

    fn cancel(&mut self, ids: &[OrderId]) -> Result<Vec<CancelResult>> {
        let results = self.with_connection(|c| c.cancel(ids))?;
        let now = self.now();
        for result in &results {
            match &result.outcome {
                // Asked, not done: the order stays, with a note.
                CancelOutcome::Requested => {
                    self.book.mark_cancel_requested(&result.order_id, now);
                    self.schedule_refetch(result.order_id.clone(), 1_500);
                }
                // The broker knows better than we do: read how it ended.
                CancelOutcome::AlreadyFilled
                | CancelOutcome::NotFound
                | CancelOutcome::BeingReplaced => {
                    self.schedule_refetch(result.order_id.clone(), 0);
                }
                CancelOutcome::Rejected(_) => {}
            }
        }
        self.dirty = true;
        Ok(results)
    }

    fn amend(&mut self, id: &OrderId, change: &OrderChange) -> Result<EditOutcome> {
        let order = self
            .book
            .get(id)
            .cloned()
            .ok_or_else(|| BrokerError::new(ErrorKind::NotFound, "That order is not open."))?;
        let now = self.now();
        let result = self.with_connection(|c| c.amend(&order, change));
        match &result {
            Ok(EditOutcome::Native) => {
                self.book
                    .mark_edit_requested(id, change.price, change.size, now);
                self.schedule_refetch(id.clone(), 1_000);
            }
            Ok(EditOutcome::ReplacedAtomically(new_id)) => {
                self.schedule_refetch(id.clone(), 0);
                self.schedule_refetch(new_id.clone(), 0);
            }
            // The edit may have applied: read the order rather than assume.
            Err(error) if error.kind == ErrorKind::OutcomeUnknown => {
                self.schedule_refetch(id.clone(), 500);
            }
            Err(_) => {}
        }
        self.dirty = true;
        result
    }
}

/// An account by id, for callers that group balances.
pub fn balances_of<'a>(
    snapshot: &'a BrokerSnapshot,
    account: &'a AccountId,
) -> impl Iterator<Item = &'a Balance> {
    snapshot
        .balances
        .iter()
        .filter(move |b| &b.account == account)
}
