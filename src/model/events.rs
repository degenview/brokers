//! What a live account stream tells the runtime.

use super::types::LiveOrder;

/// One thing an account stream reported. Adapters turn their wire messages into these; the shared
/// reducer does the rest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamEvent {
    /// The stream is connected and subscribed (after every reconnect too). Anything that happened
    /// while it was down is not in it, so the runtime re-reads state when it sees this.
    Connected,
    /// Orders as the broker reports them now. `snapshot` marks the first batch after subscribing,
    /// which describes existing orders rather than something that just happened.
    Orders {
        orders: Vec<LiveOrder>,
        snapshot: bool,
    },
    /// Messages were missed (a sequence gap). The runtime re-reads state.
    Gap,
    /// The stream is down; it will try again by itself.
    Disconnected { reason: String },
}

/// Receives stream events on the stream's own thread. Must not block.
pub trait EventSink: Send + Sync {
    fn event(&self, event: StreamEvent);
}
