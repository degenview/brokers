# Adding a broker

A broker is one module and one line in the registry. Nothing outside the module may name it (a
guard test in `lib.rs` fails the build if it does), and the application's interface needs no
change: it draws from the broker's `Descriptor` and the neutral model.

## Steps

1. **Module.** `src/<broker>/` behind a Cargo feature of the same name. Add the feature to
   `Cargo.toml`, and one `#[cfg(feature = "<broker>")]` line to `registry::all()` and `lib.rs`.
2. **Descriptor.** A `static Descriptor`: id (lower-case ASCII), display name, the credential
   fields its form collects (key, label, hint, secret?, kind), setup steps and permission note for
   the user, docs URL, `ClientIdRules` (maximum length; whether reusing an id returns the existing
   order), the market-data source whose symbols it trades, its `AccountModel`, and the minimum gap
   between requests.
3. **Signer.** Implement `auth::RequestSigner` for the way it authenticates (a signed token, an
   HMAC over the query, an HMAC with a nonce). Per-broker signers live in the broker's module.
   Never include a secret in an error; build every error through `http::HttpClient` or a
   `Scrubber`.
4. **`impl Broker`.** `resolve_market` maps a chart's market to this broker's product with no
   network call and **never substitutes** a different quote currency or venue; `connect` validates
   the credentials with reads only and returns a `Connection`.
5. **`impl Connection`.** `balances`, `products`, `product`, `quote`, `capabilities` (pure: from the
   product's rules and the credential's permissions, and honest about what is *not* possible),
   `preview`, `place`, `cancel`, `preview_edit`, `amend`, `order`, `open_orders`, `order_history`,
   `fills`, `find_by_client_id`, `fee_summary`, `open_stream`.
   * `place` is sent once. Any failure that leaves the outcome unknown (transport error, a 5xx, an
     unreadable 2xx) is `ErrorKind::OutcomeUnknown`.
   * `amend` changes the order in place, or reports an atomic replace. It never cancels and places.
   * Money is `Decimal`, parsed from the broker's strings; never round an order.
6. **Stream.** `open_stream` spawns a thread that turns the broker's account messages into
   `StreamEvent`s and reconnects by itself with `ws::Backoff`. Sign again on every reconnect.
7. **Mock exchange.** A stateful fake (REST, and a WebSocket server if there is a stream) built from
   the broker's *published specification*, labelled constructed. Implement
   `testing::conformance::Exchange` over it and call `run_all` from a test: the adapter must pass
   the same twelve scenarios as every other.
8. **Docs.** `docs/<broker>.md`: what was verified, what was inferred, what is not implemented, and
   what could not be tested against the live service.

## What the shared layers already give you

The `Book` reducer (idempotent, ordered, notifies once), the runtime (state read, stream applied
after it, re-read after gaps, sleep and unknown outcomes), the submit/reconcile flow with its
journal, validation against product rules, redacted credentials, and the HTTP client's retry and
no-redirect rules.

## What not to build

Transfers, withdrawals, deposits or conversions. Cancel-and-replace presented as an edit. An
automatic retry of a write. A base-URL override that works in a release build.
