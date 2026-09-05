# Plan 207: Invalidate session "Always" grants when autonomy is tightened at runtime

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/approval/mod.rs src/gateway/api_v1.rs src/tui/app.rs`

## Status

- **Priority**: P1 (security — a tightening does not revoke prior blanket grants)
- **Effort**: S
- **Risk**: LOW (only removes previously-granted approvals; fails safe)
- **Depends on**: none (independent of plan 205; both concern the prompt gate)
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

When an operator answers "Always" for a tool, its name is added to the
`ApprovalManager`'s session allowlist and it stops prompting for the rest of the
session. `needs_approval` checks `forces_prompt` (live `always_ask`) **before**
the session allowlist, but the session allowlist itself is **never cleared when
the policy changes**. So:

- Owner grants "Always" for `git_operations` / `http_request` / `delegate` under
  Smart, then tightens to Manual expecting every tool to prompt again. Because
  those tools are not individually in Manual's `always_ask` (before plan 205) —
  or even after 205's wildcard, the session allowlist is checked and the
  wildcard path must still win — the stale grant can keep them running
  unprompted.
- The live-tightening tests only prove tightening works for a tool that *is* in
  the new `always_ask` (`shell`, `file_read`); the majority case is untested.

A runtime tightening must revoke prior blanket grants, or "switch to Manual" is
not actually stricter for anything the user already waved through.

## Current state

### The check order — `src/approval/mod.rs:130-161`

```rust
    pub fn needs_approval(&self, tool_name: &str) -> bool {
        let autonomy = self.effective_autonomy();
        if autonomy == AutonomyLevel::Full { return false; }
        if autonomy == AutonomyLevel::ReadOnly { return false; }
        if self.forces_prompt(tool_name) { return true; }     // always_ask (live)
        if self.pre_approved(tool_name) { return false; }     // auto_approve (live)
        let allowlist = self.session_allowlist.lock();
        if allowlist.contains(tool_name) { return false; }    // <-- stale "Always" grants
        true
    }
```

### The session allowlist is never cleared on a policy change — `src/approval/mod.rs:196-207`

`session_allowlist` is a `Mutex<HashSet<String>>` populated by `record_decision`
(on "Always") and `seed_session_allowlist`; there is no `clear` and no hook that
runs when the live policy is swapped.

### Two surfaces populate it differently

- **TUI**: one `ApprovalManager` lives for the session; grants accumulate; a
  `reload_config` swaps the policy but not the manager.
- **Web console**: a fresh `ApprovalManager` is built per SSE turn
  (`src/gateway/api_v1.rs:629`) and **re-seeded** from a per-session store
  (`seed_session_allowlist`, `api_v1.rs:635`) — so a stale grant is re-seeded
  every turn unless that store is invalidated on an autonomy change.

## The fix

### Step 1 — a `clear_session_allowlist` on the manager

Add:

```rust
    /// Drop all prior "Always" session grants. Call this whenever the live
    /// policy is tightened/reloaded so a blanket grant made under a looser
    /// preset does not survive the change.
    pub fn clear_session_allowlist(&self) {
        self.session_allowlist.lock().clear();
    }
```

### Step 2 — TUI: clear on reload/tighten

In the TUI path that applies an autonomy change / `reload_config` (where the
new `SecurityPolicy` is attached — see `src/tui/app.rs` `apply_preset_to_config_and_reload`
and `reload_config`), call `clear_session_allowlist()` on the live
`ApprovalManager` after the policy swap. Only clear on an actual autonomy/policy
change, not on unrelated config reloads (key it to the autonomy fields
changing).

### Step 3 — Web console: invalidate the per-session grant store on autonomy change

The web per-session store that feeds `seed_session_allowlist` must be cleared
when `PUT /api/v1/config/autonomy` changes autonomy, so a stale grant is not
re-seeded next turn. Locate the per-session "Always" store (the source passed to
`seed_session_allowlist` at `api_v1.rs:635`) and clear/rev it on the autonomy
PUT. If that store is keyed per session id, clear the entries; if a full
invalidation is simpler and correct, do that.

If wiring the web store invalidation is non-trivial, implement Steps 1–2
(TUI + the manager method) and DEFER Step 3 with a clear note — the TUI is the
primary interactive surface and the manager method is the reusable primitive.

## Files

- **In scope**: `src/approval/mod.rs` (the method), `src/tui/app.rs` (call on
  reload), and the web per-session grant store (Step 3).
- **Out of scope**: the Manual preset semantics (plan 205), the propagation of
  autonomy changes to channel listeners (plan 211), the config-watcher vs
  forced-reload issue (plan 211).

## STOP conditions

- If the session allowlist is already cleared on reload somewhere (search for
  `session_allowlist` writers), this may be partly done — reconcile.
- If clearing on every `reload_config` would drop grants on unrelated config
  changes (noisy), scope the clear to autonomy-field changes only and report.

## Done criteria

1. `cargo fmt`/`clippy`/`cargo test -p rantaiclaw --lib approval` clean.
2. New test proving a tightening revokes a prior blanket grant:

```rust
#[test]
fn always_grant_is_revoked_when_autonomy_tightens() {
    let smart = /* Supervised/Smart config */;
    let mgr = ApprovalManager::from_config(&smart.autonomy);
    mgr.record_decision("git_operations", &json!({}), ApprovalResponse::Always, "cli");
    assert!(!mgr.needs_approval("git_operations"), "granted this session");
    // Simulate a tighten-to-Manual reload:
    mgr.clear_session_allowlist();
    // with the Manual policy attached (wildcard always_ask from plan 205), or
    // simply assert the stale grant is gone:
    assert!(mgr.needs_approval("git_operations"), "tightening must re-prompt");
}
```

## Test plan

Add the test to the approval module. If plan 205 has landed, attach a Manual
policy so `forces_prompt` also returns true; if not, the `clear` alone restores
the default-supervised prompt. Note the interaction with 205 in the PR.

## Risk & rollback

- **Risk**: LOW — the change only *removes* previously-granted approvals, which
  fails safe. The only UX cost is re-prompting after a tighten, which is the
  intended behavior.
- **Rollback**: revert; no schema/config/migration change.

## Maintenance note

Any future per-session approval cache (web, channels, TUI) must be invalidated on
a policy change. Consider keying the session allowlist to a policy "generation"
counter so a stale grant is structurally impossible rather than
clear-on-reload — a good follow-up if the surfaces multiply.
