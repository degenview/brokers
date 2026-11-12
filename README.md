# Brokers Crate

Broker adapters and a trading model for [DegenView](https://github.com/degenview/degenview).
Coinbase Advanced Trade (spot) first. Blocking Rust: no UI, no database, no async runtime.

## Responsibilities

**This crate**

* Talks to the exchange: balances, markets, quotes, orders, fills, and a live account stream.
* Checks an order against the market's rules and suggests the nearest valid value. Never rounds.
* Sends an order once. If the result is unknown, only a lookup by `ClientOrderId` settles it.
* Keeps an order book in sync (REST plus stream) on one worker thread per broker.
* Keeps credentials out of errors, `Debug` output and logs.

**The application supplies**

* The UI, and the confirmation step before any order.
* `SecretStore`: where credentials live.
* `PendingJournal`: a durable record of orders sent and not yet confirmed (must survive a restart).
* `Waker` and `NoticeSink`: how the runtime asks for a redraw and reports events.

**Nobody does:** moving funds. There is no transfer, withdrawal or deposit code, and no adapter asks
for a permission that would allow it.

## Public API

| Item | What it is |
| --- | --- |
| `registry::{all, by_id, id_for}` | The brokers compiled in. |
| `Broker`, `Descriptor` | One exchange: its id, credential fields, rules; `resolve_market`, `connect`. |
| `Connection` | One blocking session: `balances`, `products`, `quote`, `preview`, `place`, `cancel`, `amend`, `order`, `open_orders`, `order_history`, `fills`, `find_by_client_id`, `open_stream`. |
| `Credentials`, `SecretStore` | A broker's secrets (redacted `Debug`) and where the app keeps them. |
| `Env` | Clock, stream timing, and loopback-only URL overrides (debug and test builds). |
| `OrderDraft`, `validate` | What to place, and the check against `ProductSpec` rules. |
| `LiveOrder`, `LiveFill`, `Balance`, `ProductSpec`, `Quote` | The model. All amounts are `Decimal`. |
| `Capabilities` | What a credential can do in a market, and why not if it can't. |
| `BrokerError`, `ErrorKind` | One error type. `OutcomeUnknown` means look the order up, don't resend. |
| `submit::{submit, reconcile, retry}` | Record, send once, settle. Used by the runtime; usable alone. |
| `BrokerRuntime`, `RuntimeConfig` | The managed path: `start`, then `submit`, `preview`, `cancel`, `amend`, ... each returning `Pending<T>`. |
| `BrokerSnapshot` | What the UI reads: `status`, `orders`, `pending`, `balances`, `can_send_orders()`. |
| `model::pending::PendingJournal` | The journal trait the app implements. `MemoryJournal` for tests. |
| `auth`, `http`, `ws`, `Scrubber` | For adapter authors: signer trait, HTTP client, WebSocket plumbing. |
| `testing` (feature) | Loopback mock exchange and the conformance suite. Tests only. |

## Brokers

| Id | Money | Credentials |
| --- | --- | --- |
| `coinbase` | real | CDP API key: `name` and `private_key` |
| `coinbase-sandbox` | none (`real_money = false`) | none; canned answers |

Details and what was verified: [docs/coinbase.md](docs/coinbase.md).
Adding a broker: [docs/adding-a-broker.md](docs/adding-a-broker.md).

## Example

```rust,no_run
use std::sync::Arc;
use degenview_brokers::model::pending::MemoryJournal;
use degenview_brokers::{registry, BrokerId, BrokerRuntime, Credentials, Env, RuntimeConfig};

let coinbase = registry::by_id(BrokerId::new("coinbase")).expect("compiled in");
let credentials = Credentials::new()
    .with("name", "organizations/…/apiKeys/…")
    .with("private_key", "-----BEGIN EC PRIVATE KEY-----\n…");

// One session, called directly.
let connection = coinbase.connect(&credentials, &Env::default())?;
for balance in connection.balances()? {
    println!("{} {}", balance.asset, balance.available);
}

// Or managed: a worker thread keeps the book current; the UI polls the snapshot.
# struct Redraw; impl degenview_brokers::Waker for Redraw { fn wake(&self) {} }
# struct Notices; impl degenview_brokers::NoticeSink for Notices { fn notice(&self, _: degenview_brokers::model::reducer::Notice) {} }
let config = RuntimeConfig::new(Env::default(), Arc::new(MemoryJournal::new()), Arc::new(Redraw), Arc::new(Notices));
let runtime = BrokerRuntime::start(coinbase, credentials, config);
let snapshot = runtime.snapshot();
println!("{:?}, can send orders: {}", snapshot.status, snapshot.can_send_orders());
# Ok::<(), degenview_brokers::BrokerError>(())
```

## Guarantees

* An order is never rounded; a value off its increment comes back with a suggestion.
* A write is sent once and never retried. Unfinished means `OutcomeUnknown`, and a 409 on a write counts as unfinished.
* An unconfirmed order belongs to the account that sent it; another account's records are held untouched.
* Requests go only to the broker's own host, redirects are not followed, and no secret reaches an error.

## Development

```bash
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo deny check
```

Tests never reach the network or a real exchange. Licensed GPL-3.0-only.
