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

## What is implemented (0.1)

Connect and validate a key; read balances (`/accounts`, paged by cursor, available and held
separately); read spot products with their increments, minimum and maximum sizes and trading flags
(`/products`, `/products/{id}` with `get_tradability_status`); read the best bid and ask
(`/best_bid_ask`); compute per-product capabilities. Orders, cancels, edits, history, fills and the
user WebSocket follow in later versions; this file is updated as each lands.

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
