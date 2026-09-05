# Plan 205: Make the "Manual" preset actually force-prompt every tool, fix the phantom tool catalog, and owner-gate `cron_remove`

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/approval/policy_writer.rs src/approval/mod.rs src/approval/guest.rs src/approval/presets/policy_manual.toml src/tools/mod.rs`

## Status

- **Priority**: P1 (security — the "Safest" preset fails open for ~40 tools)
- **Effort**: M
- **Risk**: MED (Manual will now prompt for tools it silently ran)
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

The shared tool catalog `BUILTIN_TOOLS` (9 names) drives the "Manual" preset's
`always_ask` list and the console's per-tool controls. Three of those names —
`web_search`, `send_message`, `cron_schedule` — **match no registered tool**
(the real names are `web_search_tool`, and the cron family `cron_add`/… ; there
is no `send_message` tool at all). And the list omits ~40 tools that ARE
registered (`ssh`, `pty`, `http_request`, `git_operations`, `delegate`,
`manage_permissions`, `proxy_config`, `author_skill`, `skills_install`, the
`task_*` suite, hardware tools, …).

`needs_approval` only forces a prompt when the tool is in `always_ask`
(`forces_prompt`, `approval/mod.rs:111-116`); a tool in `auto_approve` or in the
session "Always" list returns `false`. So the **Manual** preset — blurb "Prompt
for every tool call. Safest, slowest." — silently fails to re-gate every tool
outside the 9-name list. With a real tool like `http_request` in `auto_approve`
(or after one earlier "Always"), selecting Manual leaves it running with no
prompt. The "Safest" rung is the one that fails open for the highest-blast
tools.

Enumerating ~50 tool names into `always_ask` would fix it once and drift again
the next time a tool is added. This plan uses a **wildcard sentinel** so Manual
can never drift from the registry. It also fixes the phantom names and adds the
missing `cron_remove` to the owner-only denylist (a guest given `cron_remove`
can delete jobs; `cron_add`/`update`/`run` are already owner-only).

## Current state

### `BUILTIN_TOOLS` has 3 phantom names — `src/approval/policy_writer.rs:174-183`

```rust
const BUILTIN_TOOLS: [&str; 9] = [
    "shell", "file_read", "file_write",
    "web_search",       // real name: web_search_tool
    "memory_store", "memory_recall",
    "send_message",     // NO such tool
    "cron_schedule",    // NO such tool (family is cron_add/…)
    "browser",
];
```

### Manual only appends `BUILTIN_TOOLS`, never clears `auto_approve` — `src/approval/policy_writer.rs:209-228`

```rust
        PolicyPreset::Manual => {
            for tool in BUILTIN_TOOLS {
                if !config.autonomy.always_ask.iter().any(|t| t == tool) {
                    config.autonomy.always_ask.push(tool.to_string());
                }
            }
        }
        PolicyPreset::Smart => config.autonomy.always_ask.clear(),
```

### `forces_prompt` is an exact-membership test — `src/approval/mod.rs:111-116`

```rust
    fn forces_prompt(&self, tool_name: &str) -> bool {
        match &self.policy {
            Some(p) => p.fields().always_ask.iter().any(|t| t == tool_name),
            None => self.always_ask.contains(tool_name),
        }
    }
```

### `cron_remove` missing from the owner-only denylist — `src/approval/guest.rs:65-77`

Lists `cron_add`, `cron_update`, `cron_run` but not `cron_remove`.

## The fix

### Step 1 — a wildcard "prompt everything" sentinel

Define a sentinel (e.g. the literal `"*"`) that `forces_prompt` treats as "any
tool prompts":

```rust
    fn forces_prompt(&self, tool_name: &str) -> bool {
        let matches = |list: &[String]| list.iter().any(|t| t == "*" || t == tool_name);
        match &self.policy {
            Some(p) => matches(p.fields().always_ask.as_slice()),
            None => self.always_ask.contains(tool_name) || self.always_ask.contains("*"),
        }
    }
```

(Adapt the `None` arm to the `always_ask` container type.)

### Step 2 — Manual sets `always_ask = ["*"]` and clears `auto_approve`

In `apply_preset_to_config` (`policy_writer.rs:209`), replace the append-loop:

```rust
        PolicyPreset::Manual => {
            // Force EVERY tool to prompt — a wildcard so this can never drift
            // from the registry. Also clear auto_approve so a stale entry
            // can't skip the prompt under the "Safest" preset.
            config.autonomy.always_ask = vec!["*".to_string()];
            config.autonomy.auto_approve.clear();
        }
```

Do **not** edit `src/approval/presets/policy_manual.toml` for this — that file
is a `PolicyBundle` (`[autonomy]`/`[approvals]`/`[command_allowlist]`/
`[forbidden_paths]`) and has **no** `always_ask`/`auto_approve` keys. Those two
fields live in `AutonomyConfig` and are set only by `apply_preset_to_config`
code, so the Manual arm above is the single place the wildcard + cleared
`auto_approve` are established. (Verify the bundle needs no change by reading
`policy_manual.toml` — it should not mention `always_ask`.)

### Step 3 — fix the phantom names in `BUILTIN_TOOLS`

`BUILTIN_TOOLS` is still used by the console's per-tool controls and any
non-Manual logic. Correct the three names: `web_search` → `web_search_tool`,
drop `send_message` (no such tool), replace `cron_schedule` with the real cron
tool name(s) or drop it (cron gating is owner-only, so it need not be in this
UI-facing builtin list — decide and note). Add a test (Step 6) that every entry
is a real registered name.

### Step 4 — add `cron_remove` to the owner-only denylist

In `src/approval/guest.rs`, add `"cron_remove"` to `OWNER_ONLY_TOOLS` next to
`cron_add`/`cron_update`/`cron_run`.

### Step 5 — preserve the "levelToRung" round-trip

The console decides Manual-vs-Smart by whether `always_ask` is non-empty
(`claw-ui`, addressed in plan 209). `always_ask = ["*"]` is non-empty, so the
round-trip still reads back as Manual. Confirm `preset_for_autonomy` /
`levelToRung` classify `["*"]` as Manual (plan 209 hardens the claw-ui side; the
backend `preset_for_autonomy` must also treat `["*"]` as Manual — verify and fix
if it keys on a specific list).

## Files

- **In scope**: `src/approval/policy_writer.rs`, `src/approval/mod.rs`
  (`forces_prompt`), `src/approval/guest.rs`, `src/approval/presets/policy_manual.toml`.
- **Out of scope**: the claw-ui catalog + `levelToRung` (plan 209), the
  `auto_approve`/session-allowlist-survives-tightening issue (plan 207), the
  per-tool console controls beyond name correctness.

## STOP conditions

- If `forces_prompt` or `always_ask` is consumed somewhere that would treat
  `"*"` as a literal tool name (search for readers of `always_ask` across
  `src/`), update those readers to honor the wildcard OR choose a sentinel that
  can't collide (e.g. a reserved token) and report.
- If `preset_for_autonomy` classifies presets by an exact `always_ask` list that
  `["*"]` would break, fix that mapping in the same PR (it is the backend half
  of plan 209's concern).

## Done criteria

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `cargo test -p rantaiclaw --lib approval` passes with new tests.
4. New tests:

```rust
#[test]
fn manual_preset_forces_prompt_for_every_registered_tool() {
    let mut cfg = /* a config; put http_request in auto_approve first */;
    cfg.autonomy.auto_approve = vec!["http_request".into()];
    apply_preset_to_config(&mut cfg, PolicyPreset::Manual);
    let mgr = ApprovalManager::from_config(&cfg.autonomy);
    for tool in ["http_request", "ssh", "pty", "delegate", "git_operations", "shell"] {
        assert!(mgr.needs_approval(tool), "Manual must prompt for {tool}");
    }
    assert!(cfg.autonomy.auto_approve.is_empty(), "Manual must clear auto_approve");
}

#[test]
fn builtin_tools_are_all_real_registered_names() {
    let registry = /* all_tools(...) with a stub provider */;
    let real: std::collections::HashSet<_> = registry.iter().map(|t| t.name()).collect();
    for name in BUILTIN_TOOLS { assert!(real.contains(name), "phantom tool name: {name}"); }
}

#[test]
fn cron_remove_is_owner_only() {
    assert!(GuestGate::OWNER_ONLY_TOOLS.contains(&"cron_remove"));
}
```

`manual_preset_forces_prompt_for_every_registered_tool` must FAIL before Step 2.

## Test plan

Add the three tests to the approval test module. For
`builtin_tools_are_all_real_registered_names`, build the registry the way the
existing registry tests do (`src/tools/mod.rs` tests use `all_tools(...)` and
`tools.iter().map(|t| t.name())`). This test is the drift guard that keeps the
catalog honest going forward.

## Risk & rollback

- **Risk**: MED — Manual will now prompt for every tool (including ones that ran
  silently before). That is the whole point of "Safest"; call it out in the PR +
  CHANGELOG. The wildcard is a behavior change to `forces_prompt`; the STOP
  condition guards against a literal-`*` reader.
- **Rollback**: revert the four files; no schema/migration change (the
  `always_ask = ["*"]` value is data the existing schema already accepts).

## Maintenance note

The wildcard sentinel eliminates the drift class entirely: adding a new tool no
longer requires touching Manual. Keep `builtin_tools_are_all_real_registered_names`
green — it is the guard that would have caught the three phantom names.
`OWNER_ONLY_TOOLS` should gain a doc comment stating the inclusion criterion so
the next mutation tool (like `cron_remove` here, `proxy_config` in plan 201) is
not missed.
