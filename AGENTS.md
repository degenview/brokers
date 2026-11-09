# AGENTS.md — degenview-brokers

Guidance for AI coding agents working in this repository. This crate trades real money for its
users; correctness and secrecy come before convenience.

## Rules

- **No exchange name outside its adapter module** (`src/coinbase/`). The model, `adapter`, `http`,
  `registry` and everything the application sees use broker-neutral names. A guard test enforces it.
- **Money is `Decimal`.** Never `f64` for a price, size, fee or balance. Never round an order; report
  the nearest valid value as a suggestion (`validate`).
- **Secrets.** A credential, a signed token or an `Authorization` header must not reach an error, a
  `Debug`/`Display` output, a log line, a panic message or a test snapshot. Build errors through
  `http::HttpClient` (which scrubs) or `Scrubber`. `Credentials` has a redacted `Debug` on purpose.
- **Writes.** A POST/DELETE is sent once. A write that did not finish is `ErrorKind::OutcomeUnknown`;
  only a lookup by `ClientOrderId` may resolve it. Never add an automatic retry to a write.
- **Hosts.** Requests go to the adapter's own host only. Redirects are not followed. A base-URL
  override exists for debug and test builds, honours loopback addresses only, and must stay that way.
- **No funds movement.** Do not add transfer, withdrawal, deposit, convert or send endpoints, or ask
  for a permission that enables them.
- **Blocking, no async runtime.** Adapters run on the caller's worker threads.
- **Supply chain.** crates.io dependencies only, exact enough that `Cargo.lock` is reviewed, `cargo deny
  check` clean, every workflow `uses:` pinned to a commit, job permissions minimal. Never run
  `cargo publish` or push a tag: the maintainer does.
- **Tests** use the loopback mock (`testing`); fixtures are built from the exchange's published
  specification and are labelled constructed. Never place a real order from a test.

## Layout

```
src/model/       pure types, validation, capabilities
src/adapter.rs   Broker, Connection, Descriptor, Env
src/auth/        the RequestSigner trait (a broker's signer lives in its module)
src/http.rs      the HTTP client;  src/ws.rs  blocking WebSocket plumbing
src/submit.rs    record, send once, reconcile
src/model/reducer.rs, src/runtime.rs   the shared book and the per-broker runtime
src/coinbase/    the Coinbase Advanced Trade adapter (feature `coinbase`), with its fake exchange
src/testing/     loopback mock server and the conformance suite (feature `testing`)
docs/            coinbase.md (what was verified), adding-a-broker.md
```

## Before a change is done

`cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`,
`cargo deny check`. A new public item is documented; a behaviour that is inferred rather than
verified against the exchange is said so in `docs/` and in the item's doc comment.
