# Plan 041: Gate the write-side skill tools on autonomy (ReadOnly) and make them owner-only for guests

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 4736e2e..HEAD -- src/tools/mod.rs src/tools/skills_install.rs src/tools/author_skill.rs src/approval/guest.rs`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `4736e2e`, 2026-07-23 (branch `feat/web-approval-parity`)

## Why this matters

The three write-side skill tools — `author_skill`, `skills_install`,
`skills_install_deps` — bypass two gates that every other mutating tool
respects:

1. **The ReadOnly autonomy gate.** `FileWriteTool` (and other act-tools) call
   `self.security.can_act()` and refuse in read-only/plan mode. The skill tools
   are constructed with **no `SecurityPolicy` at all**, so they can't check.
   `ApprovalPolicy::needs_approval` returns `false` for `AutonomyLevel::ReadOnly`
   ("ReadOnly blocks everything — handled elsewhere"), and "elsewhere" is
   exactly the per-tool `can_act()` check these tools lack. Net: in read-only
   mode the agent still authors skills, installs arbitrary ClawHub skills, and
   runs package managers via `skills_install_deps` — with **no prompt**. They
   also skip the sliding-window rate limiter that `file_write` enforces via
   `record_action()`.
2. **The guest owner-only gate.** `GuestGate::OWNER_ONLY_TOOLS` hard-denies a
   fixed set of authority-escalating tools to non-owners regardless of
   `guest_allowed_tools`. The skill tools are **not** in that list. Installing
   or authoring a skill injects instructions into the system prompt for every
   subsequent turn — a persistent prompt-injection / capability-escalation
   primitive. An owner who adds `skills_install` to `guest_allowed_tools` (to
   let guests pull, say, a weather skill) unknowingly hands guests that
   primitive.

This plan makes the skill tools mirror `file_write`'s `can_act()` +
`record_action()` gate, and adds them to `OWNER_ONLY_TOOLS`. Both changes only
*tighten* — they add no capability.

## Current state

Files:

- `src/tools/mod.rs` — the tool registry. `all_tools_with_runtime` already
  receives `security: &Arc<SecurityPolicy>` (line 257). The three skill tools
  are constructed at 400–410 **without** `security`.
- `src/tools/skills_install.rs` — `SkillsInstallTool` (29–107) and
  `SkillsInstallDepsTool` (111–249). Constructors take no `SecurityPolicy`.
- `src/tools/author_skill.rs` — `AuthorSkillTool` (41–52). Constructor takes
  only `skills_dir: PathBuf`.
- `src/tools/file_write.rs` — the **exemplar** gate (55–73, 146).
- `src/approval/guest.rs` — `OWNER_ONLY_TOOLS` (53–59) and `tool_permitted`
  (61–68).
- `src/approval/mod.rs` — `needs_approval` (86–115).
- `src/security/policy.rs` — `can_act` (851–853) returns `false` in ReadOnly;
  `record_action` (889–892) returns `false` when the hourly budget is exhausted.

The three tools are built **without** `security` (`src/tools/mod.rs:400-410`):

```rust
    if let Ok(active_profile) = crate::profile::ProfileManager::active() {
        tool_arcs.push(Arc::new(author_skill::AuthorSkillTool::new(
            active_profile.skills_dir(),
        )));
        tool_arcs.push(Arc::new(skills_install::SkillsInstallTool::new(
            active_profile,
        )));
    }
    tool_arcs.push(Arc::new(skills_install::SkillsInstallDepsTool::new(
        workspace_dir.to_path_buf(),
        config.clone(),
    )));
```

The exemplar gate at the top of `FileWriteTool::execute`
(`src/tools/file_write.rs:57-73`, plus the post-check record at line 146):

```rust
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if self.security.is_rate_limited() {
            return Ok(ToolResult { /* … rate-limit error … */ });
        }
        // …later, once the write is about to happen…
        if !self.security.record_action() { /* … rate-limit error … */ }
```

`can_act` (`src/security/policy.rs:851-853`):

```rust
    pub fn can_act(&self) -> bool {
        self.effective_autonomy() != AutonomyLevel::ReadOnly
    }
```

`needs_approval` short-circuits ReadOnly to `false`
(`src/approval/mod.rs:92-95`) — confirming the block must come from the tool
itself:

```rust
        // ReadOnly blocks everything — handled elsewhere; no prompt needed.
        if self.autonomy_level == AutonomyLevel::ReadOnly {
            return false;
        }
```

`OWNER_ONLY_TOOLS` + `tool_permitted` (`src/approval/guest.rs:53-68`):

```rust
    pub const OWNER_ONLY_TOOLS: &'static [&'static str] = &[
        "manage_permissions",
        "issue_pairing_code",
        "delegate",
        "ssh",
        "pty",
    ];

    pub fn tool_permitted(&self, tool: &str) -> bool {
        if Self::OWNER_ONLY_TOOLS.contains(&tool) {
            return false;
        }
        self.permitted_tools.contains(tool)
    }
```

`GuestGate::new(auto_approve, guest_tools, guest_commands)` (`guest.rs:29-39`)
takes the permitted set as its arguments — the test in Step 4 uses this.

Repo security posture (CLAUDE.md §3.5/§3.6): local-capability tools are
usable-by-default, but where an autonomy/owner gate pattern already exists
(`can_act`, `OWNER_ONLY_TOOLS`), mutating tools must honour it. This plan adds
no config keys and widens nothing — it only aligns three tools with the
existing gates, so no schema-version bump.

## Commands you will need

| Purpose        | Command                                                     | Expected on success |
|----------------|------------------------------------------------------------|---------------------|
| Build          | `cargo build`                                              | exit 0              |
| Format check   | `cargo fmt --all -- --check`                               | exit 0, no diff     |
| Lint           | `cargo clippy --all-targets -- -D warnings`                | exit 0, no warnings |
| Tests (scoped) | `cargo test --lib skills_install`                          | all pass, incl. new |
| Tests (guest)  | `cargo test --lib guest`                                   | all pass, incl. new |

Full `cargo test` is disk-heavy — prefer `--lib` with a filter. `strict-clippy-delta`
and `setup_e2e` run POST-merge; run the scoped clippy above before merge.

## Scope

**In scope** (the only files you should modify):

- `src/tools/skills_install.rs` — add `security: Arc<SecurityPolicy>` to both
  `SkillsInstallTool` and `SkillsInstallDepsTool`; add the `can_act()` +
  `record_action()` gate to both `execute` methods; add a ReadOnly-block test.
- `src/tools/author_skill.rs` — same for `AuthorSkillTool`.
- `src/tools/mod.rs` — pass `security.clone()` into the three constructors
  (400–410).
- `src/approval/guest.rs` — add the three tool names to `OWNER_ONLY_TOOLS`; add
  a guest-denied test.

**Out of scope** (do NOT touch):

- `apply_preset_tool_filter` (`src/tools/mod.rs:234-251`). The alternative
  "extend the Strict filter" approach is intentionally **not** taken — it would
  only cover the Strict *preset*, not `AutonomyLevel::ReadOnly`. The `can_act()`
  gate is the correct, complete fix; leave the preset filter as-is.
- The read-side skill tools (`skills_list`, `skill_view`, `skills_search`) —
  read-only, correctly ungated.
- `ApprovalPolicy` / `needs_approval` — do not change the ReadOnly short-circuit.

## Git workflow

- Branch: `advisor/041-skill-write-tools-gating`
- Conventional commits, e.g.
  `fix(tools): gate skill write-tools on can_act and make them owner-only`
- **Do NOT add a `Co-Authored-By` trailer** (repo rule).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Thread `SecurityPolicy` into the three tool constructors

In `src/tools/skills_install.rs`, add a field + constructor arg to each tool.
`use std::sync::Arc;` is already imported. Add
`use crate::security::SecurityPolicy;` (or reference the fully-qualified type):

```rust
pub struct SkillsInstallTool {
    profile: crate::profile::Profile,
    security: Arc<crate::security::SecurityPolicy>,
}
impl SkillsInstallTool {
    pub fn new(profile: crate::profile::Profile,
               security: Arc<crate::security::SecurityPolicy>) -> Self {
        Self { profile, security }
    }
}
```

Do the same for `SkillsInstallDepsTool` (add `security` field + arg). In
`src/tools/author_skill.rs`, add the `security` field + arg to
`AuthorSkillTool::new` (keep the existing `skills_dir`).

Confirm the exact `SecurityPolicy` path — grep for how `file_write.rs` names it
(`self.security: Arc<SecurityPolicy>`); match that import.

**Verify**: `cargo build` will fail here (callers not updated yet) — that is
expected; proceed to Step 2 before re-checking.

### Step 2: Pass `security.clone()` at the registry (`src/tools/mod.rs:400-410`)

```rust
    if let Ok(active_profile) = crate::profile::ProfileManager::active() {
        tool_arcs.push(Arc::new(author_skill::AuthorSkillTool::new(
            active_profile.skills_dir(),
            security.clone(),
        )));
        tool_arcs.push(Arc::new(skills_install::SkillsInstallTool::new(
            active_profile,
            security.clone(),
        )));
    }
    tool_arcs.push(Arc::new(skills_install::SkillsInstallDepsTool::new(
        workspace_dir.to_path_buf(),
        config.clone(),
        security.clone(),
    )));
```

**Verify**: `cargo build` → exit 0.

### Step 3: Add the `can_act()` + `record_action()` gate to each `execute`

At the **top** of each of the three `execute` methods (before any filesystem or
network work), mirror `file_write.rs:57-73`:

```rust
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }
        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour.".into()),
            });
        }
```

Place it after cheap arg validation is fine, but it MUST be before
`clawhub::install_one`, `install_deps_for_with_prefs`, or the `SKILL.md` write.
Update the existing tests that construct these tools (e.g. the ones at
`skills_install.rs:271-343` and `author_skill.rs:402-553`) to pass a
`SecurityPolicy` — use a permissive one so those tests still exercise the happy
path. Look at how other tool tests build a `SecurityPolicy` (e.g.
`SecurityPolicy::from_config(...)` with a default `AutonomyConfig`, or a
`Full`-autonomy policy) and reuse that constructor.

**Verify**: `cargo build` → exit 0; `cargo test --lib skills_install` → all
existing tests still pass (after updating their constructors).

### Step 4: Add the three tools to `OWNER_ONLY_TOOLS`

In `src/approval/guest.rs`, extend the const (53–59):

```rust
    pub const OWNER_ONLY_TOOLS: &'static [&'static str] = &[
        "manage_permissions",
        "issue_pairing_code",
        "delegate",
        "ssh",
        "pty",
        // Skill write-tools inject instructions into the system prompt for
        // all later turns (author/install) or run package managers
        // (install_deps) — a persistent capability-escalation primitive a
        // guest must never reach, even if an owner adds it to
        // `guest_allowed_tools`.
        "author_skill",
        "skills_install",
        "skills_install_deps",
    ];
```

Update the doc-comment above the const (43–52) to mention the new class
("injects instructions the guest gate can't later constrain").

**Verify**: `cargo build` → exit 0.

### Step 5: New tests

In `src/approval/guest.rs` `#[cfg(test)]`, add (model after the existing
`tool_permitted` tests in that module):

```rust
#[test]
fn guest_denied_skill_write_tools_even_when_allowlisted() {
    // Owner explicitly (mis)configured these into guest_allowed_tools.
    let gate = GuestGate::new(
        std::iter::empty::<String>(),
        &["skills_install".into(), "author_skill".into(), "skills_install_deps".into()],
        &[],
    );
    assert!(!gate.tool_permitted("skills_install"));
    assert!(!gate.tool_permitted("author_skill"));
    assert!(!gate.tool_permitted("skills_install_deps"));
}
```

In `src/tools/skills_install.rs` `#[cfg(test)]`, add a ReadOnly-block test.
Construct a read-only `SecurityPolicy` (autonomy = `ReadOnly`; check how
`AutonomyConfig`/`SecurityPolicy` is built in existing tool tests and set the
read-only level the same way `can_act` reads it), build
`SkillsInstallDepsTool`, and assert the call is blocked:

```rust
#[tokio::test]
async fn readonly_blocks_skills_install_deps() {
    // …build a ReadOnly SecurityPolicy (autonomy read-only)…
    let tool = SkillsInstallDepsTool::new(workspace, config, readonly_security);
    let result = tool.execute(json!({"name": "anything"})).await.unwrap();
    assert!(!result.success);
    assert!(result.error.as_deref().unwrap_or("").contains("read-only"));
}
```

**Verify**: `cargo test --lib skills_install` and `cargo test --lib guest` →
all pass, including the new tests.

## Test plan

- `src/approval/guest.rs`: `guest_denied_skill_write_tools_even_when_allowlisted`
  — the three tools are denied to guests even when present in
  `guest_allowed_tools`.
- `src/tools/skills_install.rs`: `readonly_blocks_skills_install_deps` — a
  read-only autonomy policy blocks the deps install before any recipe runs.
- Existing tool tests updated to pass a permissive `SecurityPolicy` and still
  pass (happy path).
- Structural pattern: `file_write.rs` for the gate; the existing
  `tool_permitted` tests in `guest.rs` for the guest test.
- Verification: `cargo test --lib skills_install` + `cargo test --lib guest`.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo build` exits 0
- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib skills_install` and `cargo test --lib guest` pass, with
      the 2 new tests present
- [ ] `GuestGate::OWNER_ONLY_TOOLS` contains `author_skill`, `skills_install`,
      `skills_install_deps`
- [ ] All three tools call `self.security.can_act()` and return the read-only
      error string when it is false (grep: `grep -n "can_act" src/tools/skills_install.rs src/tools/author_skill.rs` → 3 hits)
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report (do not improvise) if:

- Any "Current state" excerpt doesn't match the live code (drift since `4736e2e`).
- `all_tools_with_runtime` no longer has a `security: &Arc<SecurityPolicy>`
  parameter to clone from.
- Building a `ReadOnly` `SecurityPolicy` in a test is non-obvious — report how
  `can_act`/autonomy is configured rather than guessing (do NOT weaken
  `can_act` to make a test pass).
- A verification fails twice after a reasonable fix attempt.

## Maintenance notes

- Any *new* skill-mutating or prompt-injecting tool added later must be added to
  `OWNER_ONLY_TOOLS` and must carry the `can_act()` gate — call this out in the
  PR checklist.
- Reviewer should scrutinize: the gate is before all side effects in each tool;
  the existing happy-path tests were updated (not deleted) to pass a policy; and
  the preset-filter path was deliberately left untouched.
- Deferred: `apply_preset_tool_filter` could *additionally* drop these tools in
  the Strict preset for defence-in-depth, but the `can_act()` gate already
  covers ReadOnly autonomy; not needed for this fix.
