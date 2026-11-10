# Coinbase Advanced Trade adapter

Spot trading through the **Advanced Trade API** (`https://api.coinbase.com/api/v3/brokerage`). This
is not the Coinbase Exchange ("pro") API (`api.exchange.coinbase.com`), whose keys, hosts and shapes
differ. Sources: docs.cdp.coinbase.com, the REST OpenAPI document and the WebSocket AsyncAPI
document, read on 2026-11-09. Coinbase moves these pages; re-check before relying on a detail.

## Authentication

* CDP API keys only (legacy HMAC keys expired 2025-02-05). A key has a **name**
  (`organizations/{org}/apiKeys/{id}`) and a **private key**: a PEM `EC PRIVATE KEY` (ECDSA P-256)
  or a base64 Ed25519 key (32-byte seed, optionally followed by the 32-byte public key).
* One JWT per request: `sub` = key name, `iss` = `cdp`, `nbf` = now, `exp` = now + 120 s,
  `uri` = `"METHOD host/path"` (no scheme, no query). Headers `kid` and a random `nonce`.
  `Authorization: Bearer <jwt>`. Ed25519 tokens also carry `aud: ["cdp_service"]` as Coinbase's
  samples do. A WebSocket token has no `uri`.
* Permissions are detected with `GET /key_permissions` (`can_view`, `can_trade`, `can_transfer`,
  `portfolio_uuid`, `portfolio_type`). A CDP key is scoped to **one portfolio**; there is no
  portfolio switching inside a key.

## What is implemented

**0.1** Connect and validate a key; read balances (`/accounts`, paged by cursor, available and held
separately); read spot products with their increments, minimum and maximum sizes and trading flags
(`/products`, `/products/{id}` with `get_tradability_status`); read the best bid and ask
(`/best_bid_ask`); compute per-product capabilities.

**0.2** Orders over REST:

| Operation | Endpoint | Notes |
| --- | --- | --- |
| Preview | `POST /orders/preview` | Fees, total, estimated price, slippage, `errs` and warnings. Places nothing; treated as a read. |
| Place | `POST /orders` | Sent once, with a `client_order_id`. A lost reply, a 5xx or an unreadable 2xx is `OutcomeUnknown`. |
| Cancel | `POST /orders/batch_cancel` | In batches of 100. `success` means *requested*; the order stays live until its status says otherwise. |
| Edit preview / edit | `POST /orders/edit_preview`, `/orders/edit` | Native edit of an `OPEN` limit or stop-limit order. Never cancel-and-replace. |
| Read | `GET /orders/historical/{id}`, `/batch`, `/fills`; `GET /transaction_summary` | Cursor paging. |
| Find by client id | `/orders/historical/batch` filtered locally | The `client_order_id` query parameter of `/historical/{id}` is deprecated, so the list is searched. |

Order types: market (buy by quote or base amount, sell by base), limit GTC (optionally post-only) /
GTD / FOK, stop-limit GTC / GTD. Not implemented: immediate-or-cancel limits (`sor_limit_ioc`),
brackets and attached orders, TWAP, scaled, futures, equities.

### Submitting safely (`submit`)

The order is journalled before it is sent; it is sent once; an unfinished send ends as
`OutcomeUnknown` and only a lookup by client order id settles it; "not listed" is believed only
after two minutes; a resend is explicit, looks first, and reuses the same id. Coinbase documents
that a reused `client_order_id` returns the existing order instead of creating another; a
`DUPLICATE_CLIENT_ORDER_ID` failure is also handled (as "look it up"), because which of the two the
live service does is not verified.

What counts as "not sent", and what does not:

* **Not sent** is said only when nothing left the machine (the order could not be recorded, failed
  local validation, or could not be signed) or Coinbase clearly refused it (a 400, 401, 403, 409 or
  422, or a `success: false` answer with a reason).
* **Unknown** is anything else that happened after the request went out: a connection that failed
  part-way, a 5xx, a redirect, a status that is not on the list, a success whose body cannot be
  read, or a success without an order id. The journal entry stays and a lookup settles it.
* Once Coinbase has accepted an order, failing to update the journal never takes that back: the
  order is returned as placed (`submit::Submitted::unsettled` carries the journal's error) and the
  entry is closed on a later reconcile round.
* An unreadable journal is not an empty one. The runtime publishes `journal_error`, refuses new
  orders (`BrokerSnapshot::can_send_orders` is false) and keeps trying to read it; reading,
  cancelling and editing keep working. A pending item leaves the screen only when the journal has
  let go of it.

Other rules worth knowing:

* **A market's rules are not guessed.** If Coinbase does not state the quote increment, the base
  minimum or the quote minimum of a market, the market is not offered ("did not state all of its
  trading rules") rather than taking another field's value. This is fail-closed on a doubt that
  could not be checked against a full live product list: if real products omit one of these, they
  will appear untradable here and this section should say which.
* **Cancel batches.** A cancel of more than 100 orders is sent in batches. The results of batches
  that went through are kept; a batch whose answer was lost is reported as `Unknown` (the runtime
  reads those orders again), and later batches are reported as not sent.
* **Order updates.** A finished order is never reopened and its fill never goes down, but it does
  learn a bigger fill and, from a read of the order itself, its final fee and average price. A
  stream message that says only "open" after "cancel queued" is treated as old; the read that
  follows every cancel decides.
* **Stream errors** are scrubbed of credentials and tokens and cut to 200 characters before they
  are shown.

**0.3** The user WebSocket (`wss://advanced-trade-ws-user.coinbase.com`), the shared reducer and
the runtime:

* Subscribes to `user` and `heartbeats`, each with a freshly signed token, within the documented
  five seconds; reconnects by itself with jittered backoff, signing again each time.
* A `sequence_num` jump is reported as a gap; the runtime then re-reads state. Whether the number
  counts per connection or per product is not verified, so a false alarm only costs one re-read.
* The `user` channel gives the filled amount and what is left, not the original size, so the size
  is their sum. It carries no balances, so a fill or a finished order schedules a balance read.
* The runtime reads state (REST) and applies stream messages after it, never before, so nothing
  falls between the two; it reads again after a reconnect, a gap, the machine waking from sleep (a
  long wait for the next message, measured on its own clock), an unknown submission outcome, and
  every minute as a backstop.

The shared conformance suite (`testing::conformance`) runs sixteen scenarios against this adapter's
mock: place / partial fill / fill, duplicate and stale messages, cancel requested versus done, a
cancel racing a fill, a lost reply, a refusal, an invalid order, a stream drop, waking from sleep,
a revoked key and a rate limit, and four on the record of unconfirmed orders (unreadable later,
unreadable at start, an accepted order whose record cannot be closed, a failed lookup).

## The sandbox

`CoinbaseSandbox` (id `coinbase-sandbox`, listed after Coinbase) is Coinbase's Advanced Trade
sandbox as a broker of its own, so a test connection can never be mistaken for a real one. It is
checked against the real sandbox (`cargo run --example sandbox_check`), not against its documentation
alone.

What it is, from what the sandbox answers:

* Host `https://api-sandbox.coinbase.com`, same `/api/v3/brokerage` paths. **No authentication**: it
  answers unsigned requests, so the broker has no credential fields and nothing is signed or
  stored. Coinbase issues no "sandbox keys".
* **Canned answers.** Accounts, orders, fills and previews are fixed examples. Placing, editing or
  cancelling an order succeeds but changes nothing; the order you placed is not listed afterwards,
  and looking it up by id is refused (`invalid OrderId`). An order placed here is therefore shown
  as placed, never as resting.
* **Missing endpoints** (they answer 404): key permissions, the market list, best bid and ask, the
  fee summary. So the session is synthesised (view and trade, no transfer, one "Sandbox account"),
  the markets are a built-in pair (BTC-USD and ETH-USD, with the usual increments, not Coinbase's
  live rules), there are no prices (the order pills show no bid or ask) and no fee tiers.
* **No WebSocket.** Its stream never connects; the connection is current after each read, which
  the runtime repeats on its timer.
* It trades the markets of the `coinbase` data source, the same as Coinbase.

Not verified: that these canned answers stay as they are. The sandbox's own page documents only
accounts and orders, and says its answers are static.

## Not verified against the live service

These could not be confirmed from Coinbase's documentation and have **not** been run against a real
account. The design is safe under each possible answer; each is checked with a read-only call when a
key is available, and this list is updated when it is.

* Whether Ed25519 keys are accepted by every Advanced Trade REST endpoint. Coinbase's pages
  disagree. ECDSA is the documented-safe choice and is what the setup instructions recommend.
* Whether the signed `uri` should include the query string. The documented examples have none, so
  none is signed.
* REST rate limits and headers. No current official figure was found; the client spaces requests
  120 ms apart and backs off on 429 with `Retry-After`.
* What `/orders/edit` accepts in practice (the documentation never lists editable order types; the
  failure codes suggest limit orders, and stop-limit is offered only if `edit_preview` accepts it).
* The `order_status` query parameter's spelling for the list endpoints (`order_status=OPEN`, repeated).
* Pagination of `/products` (`limit`/`offset` is used; the page size is a guess of 1000).
* The `status` value for a live market (`online` is assumed).
* Regional availability. Nothing documents it beyond the per-product flags and the
  `GEOFENCING_RESTRICTION` order failure.

## Limits worth knowing

* **Spot only.** International-exchange perpetuals left Advanced Trade on 2026-10-01 (Coinbase moved
  them to a separate gateway); US futures and equities exist in the API but are not implemented.
* **Sandbox.** `api-sandbox.coinbase.com` returns canned, static responses chosen by an `X-Sandbox`
  header. It cannot test an order's life cycle, validation or the user WebSocket, so it is no
  substitute for a real (small) account. This crate's tests use a loopback mock built from the
  published specification instead.
* A market can be visible but not tradable for an account (`view_only`); capabilities say so.
