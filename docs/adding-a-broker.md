# Adding a broker

Stub; completed with the runtime in a later version. The shape, from the Coinbase adapter:

1. `src/<broker>/` behind a Cargo feature of the same name.
2. A static `Descriptor`: id, display name, credential fields, setup steps, the market-data source
   it trades, client-order-id rules, account model, minimum request gap.
3. `impl Broker` (`resolve_market`, `connect`) and `impl Connection` (balances, products, quote,
   capabilities, then orders).
4. A signer behind `auth::RequestSigner` if the broker signs differently.
5. Register it in `registry::all()`.
6. Tests against `testing::mock` with fixtures built from the broker's published specification.

Nothing outside the broker's module may name it.
