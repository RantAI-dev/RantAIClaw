# Plan 286: Stop the email owner gate trusting a forged `Authentication-Results` header

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the row in `plans/280-production-readiness-handoff.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0dd4c03..HEAD -- src/channels/email_channel.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P0 — BLOCKER (ledger W0-7)
- **Effort**: M
- **Risk**: MED — tightening authentication can lock out a working deployment, so the
  configuration story must land with the fix
- **Depends on**: nothing
- **Category**: security (authentication / privilege)
- **Planned at**: commit `0dd4c03`, 2026-09-04

## Why this matters

`Authentication-Results` is a header the *receiving* infrastructure writes. Anything a
sender puts in a message arrives as a header too, and this code reads the first one it
finds without checking who wrote it. Combine that with a spoofed `From:` matching an owner
address and a sender obtains owner authority — which on this product means the full tool
set, shell included.

Two further weaknesses compound it: `dmarc=pass` anywhere in the header value is accepted
regardless of which domain it refers to, and the SPF/DKIM branch matches the from-domain by
substring, so `example.com.attacker.test` satisfies a check for `example.com`.

## Current state (verified at `0dd4c03`)

```rust
// src/channels/email_channel.rs:273-294
let Some(header) = parsed.header("Authentication-Results") else { return false; };
let raw = match header.as_text() { Some(t) => t.to_lowercase(), None => return false };

if raw.contains("dmarc=pass") {
    return true;
}

// An spf/dkim pass only counts when it names the From: domain.
for method in ["spf=pass", "dkim=pass"] {
    let mut rest = raw.as_str();
    while let Some(pos) = rest.find(method) {
        let tail = &rest[pos + method.len()..];
        let clause = tail.split(';').next().unwrap_or("");
        if clause.contains(&from_domain) {
            return true;
        }
```

Note also that `require_authenticated_sender` defaults to off (`:81-82`), while the owner
path consults this function regardless (`:326-337`).

## Steps

1. **Add a trusted authserv-id to the channel config.** RFC 8601 gives every
   `Authentication-Results` header an authserv-id as its first token: the identity of the
   verifier. Add e.g. `trusted_authserv_id: Option<String>` to the email channel config in
   `src/config/schema.rs`, documented in `docs/reference/channels.md`.
   **Verify**: the schema-drift gate — a new key needs the config schema version bumped and
   a migration arm. Follow the pattern of a recent key addition rather than inventing one.

2. **Parse rather than substring-search.** Rewrite the checker to: take *all*
   `Authentication-Results` headers; keep only those whose authserv-id equals the configured
   trusted value; within those, read `dmarc=pass` / `spf=pass` / `dkim=pass` results with
   their associated identifier (`header.d=`, `smtp.mailfrom=`); and compare that identifier
   to the from-domain by **equality or a proper subdomain suffix**, never `contains`.
   **Verify**: `rg -n 'contains\(&from_domain\)' src/channels/email_channel.rs` is empty.

3. **Fail closed when unconfigured.** If `trusted_authserv_id` is unset, the owner gate must
   not accept header-based authentication at all. Log once at startup, clearly, that email
   owner recognition is disabled until it is configured. Do not silently downgrade to the
   old behaviour.
   **Verify**: a test with a valid-looking header and no configured authserv-id returns
   false.

4. **Test the attack shape, not just the happy path.** At minimum:
   (a) sender-supplied header with `dmarc=pass` and an untrusted authserv-id → rejected;
   (b) trusted authserv-id with `dmarc=pass` for a *different* domain → rejected;
   (c) `spf=pass smtp.mailfrom=example.com.attacker.test` against owner domain
   `example.com` → rejected;
   (d) genuine trusted header for the owner domain → accepted.
   **Verify**: `cargo test --lib channels::email` passes; each negative test flips to a pass
   if step 2 is reverted.

5. **Document the operator impact.** `docs/reference/channels.md` §4.9 must state that email
   owner recognition requires `trusted_authserv_id`, and how to find it (it is the first
   token of the header your own mail server writes).

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib channels::email` passes with the four new tests.
- `cargo test --test schema_drift` passes after the schema bump.

## STOP conditions

- The mail parsing crate in use cannot expose multiple headers of the same name or their
  properties → STOP and report; parsing A-R by hand from a concatenated string is how this
  bug started.
- The change would require touching the shared dispatch owner-gate (`src/approval/`) →
  STOP; that gate is correct, the email evidence feeding it is not.

## Test plan

Four table-driven cases in the `email_channel.rs` test module with fixture headers. Use
neutral placeholder domains (`example.com`, `attacker.test`) per `CLAUDE.md` §9.1 — no real
addresses.

## Maintenance note

Anything that turns a header into authority belongs behind an explicit trust configuration.
If a future change adds another header-derived identity signal, it needs the same treatment.

## Rollback

One commit plus a schema bump. The schema bump means a rollback to a pre-bump binary needs
the config migrated back — note that in the PR body per `CLAUDE.md` §3.8.
