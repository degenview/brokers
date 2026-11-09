# Security policy

## Reporting a vulnerability

Please report it privately through GitHub's private vulnerability reporting (this repository's
Security tab, "Report a vulnerability"). Do not open a public issue for it.

## What counts

- Anything that leaks a credential, a signed token or an `Authorization` header: into an error
  string, a `Debug` or `Display` output, a log line, or any other place this crate writes.
- Anything that lets a request be sent to a host other than the one the broker's adapter names.
- Any path that could place, change or cancel an order without the caller asking for it, or retry a
  write that might already have been executed.
- Any code path that could move funds off the exchange. There is none and there must never be one.

## How releases are protected

The crate is published to crates.io from a signed tag on `main` by its maintainer; the
application pins an exact version. Dependencies are checked for advisories, yanked versions,
licenses and sources on every pull request (`deny.toml`), and the workflows are audited with zizmor.
