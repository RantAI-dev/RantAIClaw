# 067 — PostgreSQL backend can only speak plaintext

- **Finding:** #24 (memory deepscan, wave 4 — independent lane)
- **Written against:** `0e458fb`
- **Risk tier:** medium (`src/memory/**`, transport security, new dependency)
- **Effort:** S → M (a dependency and a crypto-provider decision, see below)
- **Depends on:** nothing
- **Blocks:** nothing

## Problem

`PostgresMemory` connected with `postgres::NoTls`, which is not a policy choice — it is
the absence of a TLS implementation. rust-postgres defaults to `sslmode=prefer`, and
`prefer` with no TLS available resolves to **plaintext, silently**.

So the backend whose own label reads *"PostgreSQL — remote durable storage"*
(`backend.rs`) sent memory contents and the password embedded in `db_url` across the
network in the clear, with nothing said. `sslmode=require` did not save an operator who
asked for it either: with `NoTls` it simply failed, with a driver-level error rather than
anything actionable.

`db_url` is never logged, so the credential exposure was on the wire only — which is
also why nothing surfaced it.

## Approach

Supply a real connector and let the URL decide. That is the standard PostgreSQL
contract, and it needs no parsing on our side:

- `sslmode=disable` — connector never invoked, unchanged behaviour for anyone who wants it
- `sslmode=prefer` (default) — TLS when the server offers it
- `sslmode=require`, `verify-ca`, `verify-full` — insisted upon, and now able to succeed

The alternative considered and rejected: keep `NoTls` and merely *warn* that the link is
unencrypted. That makes the risk visible but does not remove it, and finding #24 is that
the backend cannot encrypt at all.

## Change

### Files in scope

- `Cargo.toml` — `tokio-postgres-rustls` and `rustls-native-certs`, both optional
- `src/memory/postgres.rs` — build and pass the connector

### Files explicitly out of scope

- Everything else. This backend is compiled out by default and is not offered by
  onboarding; the change must not reach anyone not using it.

### On adding dependencies

CLAUDE.md §10 warns against dependencies for minor convenience. This is not that:
credentials in cleartext on a network link is the failure being fixed, and there is no
way to fix it without a TLS implementation.

Cost is contained. Both crates are optional and gated behind `memory-postgres`, which is
off by default, so a default build pulls nothing new. Six packages enter the tree with
the feature on, all small and all TLS/certificate related. `rustls` itself was already a
direct dependency.

Roots come from the host trust store rather than a bundled public-root set: operators
commonly front PostgreSQL with an internal CA, which bundled public roots would reject.

## Verification

```bash
cargo build --lib                             # default build unaffected
cargo build --lib --features memory-postgres
cargo test --lib --features memory-postgres memory::postgres
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

## Test plan

`tls_connector_builds_from_the_host_trust_store` — the connector must be constructible,
since nothing about `sslmode` means anything otherwise. A failure here means the host has
no CA certificates, which is what the error message tells the operator to fix.

Verifying that `require` actually negotiates needs a live server and belongs to manual
validation, not the unit suite. Say so rather than faking it.

## What this surfaced

The first version used `rustls::ClientConfig::builder()`, which resolves the crypto
provider from the *process-level default*. Both `ring` and `aws-lc-rs` are in the tree,
so rustls cannot choose on its own, and the `install_default()` that makes it work lives
in `main.rs` — it runs for the binary and nowhere else. The unit test panicked with
`Could not automatically determine the process-level CryptoProvider`.

That panic would have reached any entry point that is not `main`. The connector now names
`ring` explicitly, matching what `main.rs` installs, so it no longer depends on
initialisation order happening somewhere else.

## Escape hatch

If the supply-chain gate rejects either crate, STOP and report rather than vendoring or
pinning around it — which crates are acceptable is the maintainers' call, not this plan's.

## Maintenance note

Anything else that opens a rustls client in this codebase should name its provider the
same way rather than leaning on `main.rs`. The process-level default is invisible
coupling that only shows up off the binary's path.

## Rollback

`git revert` restores `NoTls` and drops both dependencies. No schema, config or data
change; only how the socket is opened.
