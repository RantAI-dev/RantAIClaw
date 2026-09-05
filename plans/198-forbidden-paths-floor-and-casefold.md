# Plan 198: Enforce a non-removable `forbidden_paths` floor and case-fold the match

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/security/policy.rs src/gateway/config_api.rs`
> Compare the excerpts below against live code before editing.

## Status

- **Priority**: P0 (security — path protection can be stripped to nothing)
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

`forbidden_paths` is the denylist that stops file tools from reaching
credential dirs (`~/.ssh`, `~/.aws`, `~/.gnupg`, `/etc`, …). Two gaps make it
weaker than it looks:

1. **The list has no floor and is a full replace.** `is_path_allowed` iterates
   **only** `self.fields().forbidden_paths`; there is no hardcoded baseline.
   The protective defaults live only in the config defaults, so they are
   replaceable. The gateway's `PUT /api/v1/config/autonomy` accepts and
   **replaces** the whole vector, so an authenticated body
   `{"forbidden_paths":[],"workspace_only":false}` strips every protection —
   after which `file_read`/`file_write` may reach `/etc`, `~/.ssh`, `~/.aws`.
   The API also offers no way to *append* a single forbidden path; every write
   is a full replace, so a client meaning to add one and sending a partial list
   silently erases the rest.
2. **The match is case-sensitive.** On a case-insensitive filesystem
   (macOS/Windows), `/ETC/passwd` does not match forbidden `/etc` → bypass.

This plan makes `forbidden_paths` real: a fixed floor of system/credential
directories is **always** denied at enforcement time regardless of what config
(or the API) sets, and the prefix comparison is case-folded on
case-insensitive platforms. Linux is the primary target and is case-sensitive,
so the floor is the load-bearing half.

## Current state

### `is_path_allowed` iterates only config forbidden_paths, byte/case-sensitive — `src/security/policy.rs:912-935`

```rust
        // Block absolute paths when workspace_only is set
        if self.fields().workspace_only && Path::new(&expanded).is_absolute() {
            return false;
        }

        // Block forbidden paths using path-component-aware matching
        let expanded_path = Path::new(&expanded);
        for forbidden in &self.fields().forbidden_paths {
            let forbidden_expanded = /* ~/ -> $HOME expansion */;
            let forbidden_path = Path::new(&forbidden_expanded);
            if expanded_path.starts_with(forbidden_path) {   // case-sensitive
                return false;
            }
        }
        true
```

There is no baseline list; if `forbidden_paths` is empty, nothing is denied
(subject only to `workspace_only` for absolute paths).

### The API replaces the whole vector — `src/gateway/config_api.rs:372-383`

```rust
    if let Some(v) = body.forbidden_paths {
        cfg.autonomy.forbidden_paths = v;      // full replace, no floor, no append
    }
    ...
    if let Some(v) = body.workspace_only {
        cfg.autonomy.workspace_only = v;
    }
```

The default protective list is an inline `forbidden_paths: vec![...]` in the
`AutonomyConfig` `Default` impl (`src/config/schema.rs:2246-2265`: `/etc`,
`/root`, `~/.ssh`, `~/.aws`, `~/.gnupg`, …) — note this is an inline vec, not a
named `default_forbidden_paths()` function — and it is replaceable because it is
only a default.

## The fix

### Step 1 — a hardcoded enforcement floor in `is_path_allowed`

Add a module-level constant of always-forbidden prefixes and check it in
`is_path_allowed` **before** (or in addition to) the config list, so it applies
no matter what config/API sets:

```rust
/// Directories that are ALWAYS denied to file tools, independent of the
/// operator's `forbidden_paths` config. The config list can only ADD to this;
/// it can never remove a floor entry. Prevents an empty/relaxed
/// `forbidden_paths` (including one set via the config API) from exposing
/// credentials and system files.
const FORBIDDEN_PATH_FLOOR: &[&str] = &[
    "/etc", "/root", "/boot", "/sys", "/proc",
    "~/.ssh", "~/.aws", "~/.gnupg", "~/.config/gh", "~/.docker/config.json",
    "~/.kube", "~/.netrc",
];
```

Refactor the forbidden loop into a helper that checks a path against a list
(with the existing `~/` → `$HOME` expansion), and call it for **both**
`FORBIDDEN_PATH_FLOOR` and `self.fields().forbidden_paths`.

Keep the exact floor list conservative and aligned with the existing inline
default `forbidden_paths` vec in `schema.rs:2246-2265` — do not add
project-specific paths. If that default already contains an entry, that is fine;
the floor just guarantees it cannot be removed.

### Step 2 — case-fold the prefix comparison on case-insensitive platforms

In the shared helper, compare case-insensitively when the target platform's FS
is case-insensitive. A portable approach: lowercase both sides for the
comparison on `cfg!(any(target_os = "macos", target_os = "windows"))`, and keep
byte-exact `starts_with` on Linux. Use `Path::starts_with` semantics
(component-aware), not raw string prefix, to avoid `/etcd` matching `/etc`.

### Step 3 — keep the API honest (optional, low-risk)

`PUT /api/v1/config/autonomy` may still set `forbidden_paths`, but the floor now
guarantees it cannot reduce protection below the baseline. No API signature
change is required. If time permits, add a short doc comment at
`config_api.rs:372` noting that the floor is enforced regardless of this value
(so a future reader does not re-introduce the "read-only" assumption). Do **not**
change the request shape in this plan.

## Files

- **In scope**: `src/security/policy.rs` (the floor + the case-fold),
  optionally a one-line doc comment in `src/gateway/config_api.rs`.
- **Out of scope**: the shell tool honoring forbidden_paths (that is plan 214 —
  different surface), the claw-ui panel (plan 213), `workspace_only` semantics
  beyond the existing absolute-path block, symlink canonicalization
  (`is_resolved_path_allowed` already handles that).

## STOP conditions

- If `is_path_allowed` already consults a hardcoded floor (search for a const
  list of `/etc`/`~/.ssh`), this is partly done — reconcile, don't duplicate.
- If adding the floor breaks an existing test that deliberately sets
  `forbidden_paths = []` to allow a system path, STOP and report — that test
  encodes the behavior this plan intentionally removes; it must be updated to
  reflect the floor, not the floor removed.

## Done criteria

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `cargo test -p rantaiclaw --lib security::policy` passes with the new tests.
4. New tests:

```rust
#[test]
fn empty_forbidden_paths_still_denies_the_floor() {
    let mut p = /* build a policy */;
    p.set_forbidden_paths(vec![]);          // or however tests mutate fields
    assert!(!p.is_path_allowed("/etc/passwd"));
    assert!(!p.is_path_allowed(&format!("{}/.ssh/id_rsa", std::env::var("HOME").unwrap())));
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
fn forbidden_match_is_case_insensitive_on_case_insensitive_fs() {
    let p = /* policy with /etc forbidden (floor covers it) */;
    assert!(!p.is_path_allowed("/ETC/passwd"));
}
```

Verify `empty_forbidden_paths_still_denies_the_floor` FAILS before Step 1 and
PASSES after.

## Test plan

Add the tests to the `is_path_allowed` test area in `policy.rs`. Mirror how the
existing forbidden-path tests construct a policy and set fields.

## Risk & rollback

- **Risk**: MED — a floor that is too broad could block a legitimate workspace
  under, say, `/etc` overlays. The list is intentionally limited to system and
  credential dirs no agent workspace should sit inside. Keep it conservative.
- **Rollback**: single-file revert of `policy.rs`; no schema/migration change
  (the floor is code, not config).

## Maintenance note

The floor is the enforcement-time guarantee; `default_forbidden_paths` in
`schema.rs` remains the *editable* convenience list. When adding a new
credential location to one, consider whether it belongs in the floor (must
never be removable) or the default (operator may relax it).
