# degenview-brokers

A venue-neutral trading model and broker adapters for [DegenView](https://github.com/degenview/degenview).
No user interface, no database, no async runtime: the application supplies secret storage (and, in
later versions, a journal, an event sink and a clock) through small traits.

| Module | What it is |
| --- | --- |
| `model` | Decimal types for products, orders, fills and balances; order validation; capabilities. Pure. |
| `adapter`, `registry` | `Broker` / `Connection` traits and the list of compiled-in brokers. |
| `auth` | The `RequestSigner` trait (a broker's own signer lives in its module). |
| `http`, `ws` | The one HTTP client (signed per request, no redirects, reads retried, **writes never**) and blocking WebSocket plumbing. |
| `submit` | Record-then-send-once, and the lookup that settles an order whose outcome is unknown. |
| `model::reducer`, `runtime` | The shared book (idempotent, ordered, notifies once) and the per-broker runtime that keeps it in sync with the exchange. |
| `scrub`, `secret` | Credentials that cannot leak by `Debug`, and a scrubber for error text. |
| `coinbase` (feature) | Coinbase Advanced Trade, spot. See [docs/coinbase.md](docs/coinbase.md). |
| `testing` (feature) | A loopback mock exchange and the conformance suite every adapter must pass. Tests only. |

```rust,no_run
use degenview_brokers::{registry, Credentials, Env, ProductId};

let coinbase = registry::all().into_iter().next().unwrap();
let credentials = Credentials::new()
    .with("name", "organizations/…/apiKeys/…")
    .with("private_key", "-----BEGIN EC PRIVATE KEY-----\n…");
let connection = coinbase.connect(&credentials, &Env::default())?;
println!("{:?}", connection.session().permissions);
for balance in connection.balances()? {
    println!("{} {}", balance.asset, balance.available);
}
# Ok::<(), degenview_brokers::BrokerError>(())
```

## Guarantees

* An order is never rounded. A value off its increment is reported with the nearest valid value as a
  suggestion; the caller decides.
* A signed request is never replayed at another host, never retried if it writes, and never leaves
  an error with a credential in it.
* An order whose submission did not finish is `OutcomeUnknown`: the caller must look it up before
  doing anything else.
* There is no code that moves funds off the exchange, and the adapters ask for no permission that
  would allow it.

Adding another exchange is a descriptor and an adapter module, with no change to the model or the
application's interface: see [docs/adding-a-broker.md](docs/adding-a-broker.md).

## Development

```bash
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo deny check
```

Tests never reach the network or a real exchange. Licensed GPL-3.0-only.
