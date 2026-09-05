# Plan 050: Swap all config-derived policy fields as one unit; keep process state

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 3edb236..HEAD -- src/security/policy.rs src/security/mod.rs src/channels/mod.rs src/cron/scheduler.rs src/gateway/mod.rs src/tools/ src/channels/approval_relay.rs src/tui/commands/allowlist.rs src/approval/mod.rs`
> Compare the "Current state" excerpts against the live code. Line numbers
> drifting by a line or two while the quoted text matches is **not** a STOP —
> only a content mismatch is.

## Status

- **Priority**: P2
- **Effort**: M–L (~33 field reads + 76 construction-site migrations across 21 files).
- **Risk**: HIGH (`src/security/**`, `src/channels/**`)
- **Depends on**: `plans/046-gateway-shared-action-tracker.md` (needs
  `tracker: Arc<ActionTracker>`). Interacts with
  `plans/048-first-unallowed-basename-reads-effective-allowlist.md` — see
  "Ordering" below.
- **Category**: security
- **Planned at**: commit `3edb236`, 2026-07-28

## Why this matters

`SecurityPolicy` mixes state with two opposite lifetimes:

- **Config state** — `autonomy`, `allowed_commands`, `forbidden_paths`,
  `workspace_only`, `block_high_risk_commands`,
  `require_approval_for_medium_risk`, `max_actions_per_hour`. Must reflect
  config as it is *now*.
- **Process state** — `tracker` (rate window), `runtime_allowlist`
  (`/allow --persist` grants), `pending` (approval registry). Must survive a
  refresh or the control it implements evaporates.

Today only **two** of the eight config fields can be refreshed, via one
bespoke `Arc<RwLock<Option<T>>>` slot each (`autonomy_runtime`,
`allowed_commands_runtime`) with a setter and an `effective_*` accessor. The
other five are frozen at construction, so `PUT /api/v1/config/autonomy`
accepts `forbidden_paths`, `workspace_only`, `block_high_risk_commands`,
`require_approval_for_medium_risk`, and `max_actions_per_hour`, returns 200,
and none of them take effect on a running channels daemon until restart.

Adding five more slots is not the answer: the per-field pattern has already
produced one missed reader (`first_unallowed_basename`, the subject of plan
048), and five more slots means five more chances to miss one.

**This plan replaces N slots with one.** All config fields move into a single
`PolicyFields` struct held behind one shared slot; a refresh swaps that one
value. Process state stays exactly where it is.

### Why not a per-turn rebuild, and why not a new handle type

Both alternatives were considered and rejected on evidence:

- **Rebuild the policy per turn** discards process state along with config
  state. That is precisely the regression that made `max_actions_per_hour`
  unenforceable on the gateway (plan 046).
- **Give tools a new live-handle type** would work but is a 161-site
  migration: 72 `security: Arc<SecurityPolicy>` field declarations across 33
  files, plus 89 `self.security.` call sites.

The one-slot design needs roughly a dozen field-read changes (10 in
`src/security/policy.rs`, 1 in `src/security/mod.rs`, 1 in
`src/tools/git_operations.rs`) — the compiler enumerates them exhaustively once
the fields are private, so treat these counts as orientation, not a checklist and — critically
— **keeps the inner-`Arc` propagation that already works**: channels builds one
`Arc<SecurityPolicy>` at `src/channels/mod.rs:3243`, hands it to
`all_tools_with_runtime` at `:3271` (every tool clones that `Arc`) and stores
the same handle as `ctx.security` at `:3715`. Because the refreshable state is
*inside* that `Arc`, a boot-built tool observes the change. Any design that
swaps the **outer** `Arc` would silently stop reaching those tools.

## Current state

The two slots — `src/security/policy.rs:120` and `:131`:

```rust
    pub autonomy_runtime: Arc<RwLock<Option<AutonomyLevel>>>,
```
```rust
    pub allowed_commands_runtime: Arc<RwLock<Option<Vec<String>>>>,
```

Seven of the eight config fields and every read of each (verified counts). The
eighth, `max_cost_per_day_cents`, has no production reader at all — omitted
from this table deliberately, not by oversight. This is the complete
field-read surface:

| Field | `self.<field>` reads | Where |
|---|---|---|
| `autonomy` | 1 | `policy.rs:638` (inside `effective_autonomy`) |
| `allowed_commands` | 3 | `policy.rs:655`, `:732`, `:1076` |
| `forbidden_paths` | 1 | `policy.rs:858` |
| `workspace_only` | 1 | `policy.rs:852` |
| `block_high_risk_commands` | 1 | `policy.rs:595` |
| `require_approval_for_medium_risk` | 1 | `policy.rs:608` |
| `max_actions_per_hour` | 2 | `policy.rs:931`, `:936` |

Plus one direct field read in the parent module —
`src/security/mod.rs:70` (`assert_eq!(policy.autonomy, …)`) — and the
`effective_autonomy()` callers, which Step 2 must also convert:
`src/channels/mod.rs:579`, `:5263`, `:5285` (in scope), and one outside the
module — `src/tools/git_operations.rs:529`:

```rust
            match self.security.effective_autonomy() {
```

and one display read — `src/tui/commands/allowlist.rs:170` (plan 048 already
changes this line; see "Ordering").

The channels reload, which patches two of eight —
`src/channels/mod.rs:671-676`, inside `maybe_apply_runtime_config_update`
(fn at `:633`, called per inbound message at `:1699` and per dispatch-loop
iteration at `:2143`):

```rust
    ctx.security
        .set_allowed_commands(next_defaults.allowed_commands.as_ref().clone());
```
```rust
    ctx.security.set_autonomy(next_defaults.autonomy_level);
```

`ChannelRuntimeDefaults` (`src/channels/mod.rs:167-190`) carries only
`allowed_commands` and `autonomy_level` as couriers for those two setters. It
has **no** `AutonomyConfig`. Step 3 must add one.

The cron scheduler's policy, built **before** its poll loop (loop opens at
`:37`) — `src/cron/scheduler.rs:27-30`:

```rust
    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));
```

Process state that must survive — `src/security/policy.rs:98`, `:103`, `:114`:

```rust
    pub tracker: Arc<ActionTracker>,
```
(this is `pub tracker: ActionTracker` until plan 046 lands — see STOP conditions)
```rust
    pub runtime_allowlist: Arc<RwLock<HashSet<String>>>,
```
```rust
    pub pending: Arc<RwLock<Option<Arc<PendingApprovals>>>>,
```

Repo conventions:

- `parking_lot` locks (`src/security/policy.rs:1`); guards do not return
  `Result`. Never hold a guard across `.await` — resolve to an owned value.
- Tests live in-file under `#[cfg(test)] mod tests`.

### Ordering

- **After 046.** Needs `tracker: Arc<ActionTracker>`.
- **After 048, or accept a trivial conflict.** Plan 048 rewrites
  `first_unallowed_basename` to call `effective_allowed_commands()` and changes
  `allowlist.rs:170`. This plan deletes that accessor. If 048 lands first, Step
  2 converts its call to `self.fields().allowed_commands` — a one-line follow.
  If this plan lands first, 048's Step 1 should read `self.fields()` instead.
  Either order works; do not skip 048.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0, no output |
| Lint (same as CI) | `cargo clippy --locked --all-targets -- -D clippy::correctness` | exit 0 |
| Compile incl. tests | `cargo check --all-targets` | exit 0 |
| Unit tests | `cargo test --lib` | exit 0, all pass |
| Focused | `cargo test --lib security::policy` / `--lib channels` / `--lib cron` | all pass |

Note: CI also runs a **strict-delta** clippy gate
(`scripts/ci/rust_strict_delta_gate.sh`) at `-D warnings` — restricted to the
lines your diff touches, with pedantic lints on. The table's
`-D clippy::correctness` will not catch those. Before pushing, re-run clippy
at `-D warnings` and check that no warning points at a line you added.

Note: some `skills::tests::toml_*` tests are non-hermetic against `$HOME` on
some machines. If they fail, confirm they also fail on an unmodified checkout
before treating it as your regression.

**Grep form matters**: `grep -c "pat" src/` on a *directory* prints `0` and
exits 2 — it silently "passes". Every check below uses `grep -rn … | wc -l`.

## Scope

**In scope**:

- `src/security/policy.rs`
- `src/security/mod.rs` — its test
  `reexported_policy_and_pairing_types_are_usable` reads `policy.autonomy`
  directly at `:70`. Rust privacy does not reach the parent module and the
  field ceases to exist anyway, so this **must** be converted to
  `policy.fields().autonomy` or the tree will not compile.
- `src/channels/mod.rs`
- `src/cron/scheduler.rs`
- `src/gateway/mod.rs`
- `src/tools/git_operations.rs` — one `effective_autonomy()` call site
- **`src/tools/**` and `src/channels/approval_relay.rs` (test modules)** — 41
  `SecurityPolicy` struct literals across 20 files construct the policy by
  naming fields that this plan moves into `PolicyFields`. 40 are in
  `src/tools/`; **one is at `src/channels/approval_relay.rs:547`**.
- **`src/tui/commands/allowlist.rs`** — `:170` is the second *external direct
  reader* of a field this plan privatises (`security.allowed_commands`).
  Without it the tree cannot compile, whichever order you run 048 in.
- **`src/approval/mod.rs`** — only if plan 047 landed first, and then **two**
  edits are required, not one: (a) the `SecurityPolicy { … }` literal 047's
  test plan adds, migrated to the builders like any other; (b) 047 introduces
  a **production** call to `policy.effective_autonomy()` inside
  `ApprovalManager`, and Step 2 of this plan deletes that accessor — convert
  it to `policy.fields().autonomy`. They must be migrated; see Step 2b. This is unavoidable, not
  optional: once a field moves, `SecurityPolicy { autonomy: … }` is a
  **missing-field** error regardless of privacy.
- `plans/README.md` — append this row. The table header
  (`plans/README.md:245`) has **8 columns**; match it exactly:

  ```
  | 050 | Swap all config-derived policy fields as one unit; keep process state | P2 | M–L | HIGH | 046 | security | TODO |
  ```

**Out of scope** (do NOT touch):

- `src/gateway/config_api.rs` — it contains an axum handler *named*
  `set_autonomy` that is unrelated to the policy setter. Leave it.
- The ~72 `security: Arc<SecurityPolicy>` tool **field declarations and method
  calls**. This design exists specifically to avoid touching those. Only the 41
  *construction* sites in test modules change (Step 2b) — the type of the field
  itself does not.
- `src/agent/agent.rs`, `src/agent/loop_.rs` — they build a policy per Agent /
  per process; migrating them is a follow-up.
- `src/approval/mod.rs` **beyond** the edits named in the In-scope list and in
  Step 6 (047's `set_autonomy` test call sites).
  `ApprovalManager`'s own staleness is plan 047's job — do not extend it here.
- `apply_preset_tool_filter` / the tool registry — plan 051.
- `workspace_dir`. It is config-derived but changing it mid-session is not a
  supported operation; leaving it a plain field keeps this change tight. Note
  it in your report.

## Git workflow

- Branch: `refactor/policy-fields-single-slot`
- One commit per step, **except** Steps 1 through 2c: privatising the fields
  breaks every construction site at once, so those form a single indivisible
  commit. The tree compiles at the end of Step 2c and after every step from
  Step 3 onward.
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Introduce `PolicyFields` and the slot

In `src/security/policy.rs`, add:

**Trap — do not "simplify" `SecurityPolicy::default()`.** It uses
`max_actions_per_hour: 20` (`src/security/policy.rs:177`) while
`AutonomyConfig::default()` uses `200` (`src/config/schema.rs:2221`), and their
allowlists differ too — compare `policy.rs:141-153` against
`schema.rs:2187-2199` before assuming they are interchangeable. Building the default `PolicyFields` from
`AutonomyConfig::default()` would silently change the budget and allowlist
under dozens of tool tests. Preserve the existing `Default` values verbatim.

```rust
/// The config-derived half of a policy, swapped as one unit on reload.
///
/// Everything here comes from `[autonomy]` in `config.toml`. Process state —
/// the rate-limit window, operator-granted allowlist, approval registry —
/// deliberately lives on `SecurityPolicy` itself and is NOT part of this
/// struct, because a refresh must not reset it.
#[derive(Debug, Clone)]
pub struct PolicyFields {
    pub autonomy: AutonomyLevel,
    pub allowed_commands: Vec<String>,
    pub forbidden_paths: Vec<String>,
    pub workspace_only: bool,
    pub block_high_risk_commands: bool,
    pub require_approval_for_medium_risk: bool,
    pub max_actions_per_hour: u32,
    pub max_cost_per_day_cents: u32,
}

impl PolicyFields {
    fn from_config(c: &crate::config::AutonomyConfig) -> Self { /* field-by-field */ }
}
```

Replace those eight fields on `SecurityPolicy` with one slot:

```rust
    fields: Arc<RwLock<Arc<PolicyFields>>>,
```

Add the reader and the refresher:

```rust
    /// Cheap snapshot of the config half. Never hold the guard — this clones
    /// an `Arc` and releases immediately.
    pub fn fields(&self) -> Arc<PolicyFields> {
        Arc::clone(&self.fields.read())
    }

    /// Apply a config change to this running policy. Shared across every
    /// clone via the inner `Arc`, so tools already built observe it.
    /// Process state is untouched by design.
    pub fn apply_config(&self, config: &crate::config::AutonomyConfig) {
        *self.fields.write() = Arc::new(PolicyFields::from_config(config));
    }
```

The fields become private, so any outside reader must go through `fields()` —
the compiler now enforces what plan 048 had to enforce by review.

**Verify**: `cargo check --all-targets` → expect errors at the 12 read sites,
the two constructors, **the 41 struct literals in `src/tools/**` tests** that
Step 2b migrates, **and 35 more literals inside `src/security/policy.rs`'s own
test module** (from `:1090`): `:1098, 1105, 1232, 1312, 1324, 1341, 1357, 1432,
1452, 1466, 1514, 1524, 1611, 1622, 1634, 1675, 1692, 1944, 1954, 1965, 1976,
1985, 1998, 2010, 2021, 2068, 2089, 2116, 2126, 2179, 2206, 2238, 2279, 2291,
2304`, plus **21 direct reads of moved fields** in that same test module —
`:1198, 1565, 1566, 1567, 1568, 1569, 1570, 1571, 1572, 1581, 1582, 1583, 1584,
1585, 1586, 1587, 1589, 2143, 2154, 2161, 2349` (**20 net**; `:1198` is in a test
deleted in Step 2c). Two tests break wholesale and are named nowhere else in this
plan: `from_config_maps_all_fields` (`:1565-1573`) and
`default_policy_has_sane_values` (`:1581-1589`). Both convert mechanically to
`policy.fields().<field>`.

**`:1573` is deliberately absent from that list.** It reads `policy.workspace_dir`,
which is **not** a moved field — it stays plain on `SecurityPolicy`. Writing
`policy.fields().workspace_dir` there will not compile against the `PolicyFields`
declared above. `from_config_maps_all_fields` has nine consecutive assertions but
only **eight** of them change. The real literal count is
**76** (41 outside `src/security/` + 35 inside), not 41. That whole set is the
known migration surface, not a problem — do not treat the volume as a drift
signal.

(`:1093`, `:1097` and `:1104` also match a naive `grep "SecurityPolicy {"`, but
they are `fn … -> SecurityPolicy {` signatures, not literals. Brace-match before
counting.)

Most of the 35 are `..SecurityPolicy::default()` / `..default_policy()` spreads
needing no change beyond the moved fields they actually name. **Two fields named
in that set have no builder among the six in Step 2b**, so add two more:

| Field | Literals that name it | Builder to add |
|---|---|---|
| `require_approval_for_medium_risk` | `:1432`, `:1466` | `with_require_approval_for_medium_risk(bool)` |
| `block_high_risk_commands` | `:1452` | `with_block_high_risk_commands(bool)` |

Add both rather than tripping STOP condition 4 — mechanical additions in the
same `impl` block, following the `with_workspace_only` shape exactly, not design
decisions. That brings the builder count to **eight**. Do **not** add
`with_max_cost_per_day_cents`: no `SecurityPolicy { … }` literal anywhere names
that field (`:1557` and `:2040` set it inside `crate::config::AutonomyConfig`
literals opening at `:1551` and `:2034`, which this plan does not touch), so the
builder would be dead code.

`policy_dir` and `workspace_dir` stay plain fields, so `:2126`, `:2179`, `:2206`,
`:2238`, `:2291` and `:2304` need nothing beyond what `with_workspace_dir`
already covers.

### Step 2: Convert the 12 readers

The tree does **not** compile from here until Step 2c finishes. That is
expected, not drift — the fields are private and every construction site is
still a literal.

Change each `self.<field>` read listed in the "Current state" table to
`self.fields().<field>`. In functions that read several fields or read in a
loop, bind once at the top (`let f = self.fields();`) and use `f.<field>` — one
lock acquisition, not one per read.

Delete `effective_autonomy` and `effective_allowed_commands`; replace their
call sites with `self.fields().autonomy` / `self.fields().allowed_commands`.
Convert `src/tools/git_operations.rs:529` to `self.security.fields().autonomy`.

`AutonomyLevel` is `Copy`, so `self.fields().autonomy` is fine. `Vec` fields
are **not**: `self.fields().allowed_commands` is an E0507 move-out-of-`Arc`.
Bind the snapshot first (`let f = self.fields();`) and borrow, or `.clone()`.

**Verify**: `cargo check --all-targets` → the 12 reader errors are gone. What
remains is the 76 construction sites (Steps 2b and 2c), the 21 in-module field
reads, and three tests calling the accessors just deleted — all owned by
Step 2c. Do **not** expect exit 0 here.

### Step 2b: Migrate the 41 construction sites

Moving the eight fields into `PolicyFields` breaks every struct literal that
names one. Measured on the current tree: **41 `SecurityPolicy { … }` literals
across 20 files outside `src/security/`**, all in `#[cfg(test)] mod tests`,
with **74 named-field occurrences** across six distinct fields (66 written
`field: value`, plus 8 as Rust field-init shorthand — `autonomy,` at
`file_read.rs:227`, `file_write.rs:189`, `glob_search.rs:199`, `shell.rs:611`;
`max_actions_per_hour,` at `file_read.rs:227`, `file_write.rs:189`,
`glob_search.rs:199`, `pushover.rs:225`. A `grep 'field:'` misses shorthands):

| Field named in a literal | Occurrences | Moving into `PolicyFields`? |
|---|---|---|
| `autonomy` | 35 | yes |
| `workspace_dir` | 19 | **no** — stays a plain field |
| `max_actions_per_hour` | 12 | yes |
| `allowed_commands` | 6 | yes |
| `forbidden_paths` | 1 | yes |
| `workspace_only` | 1 | yes |

Counts are from the live tree; re-run them yourself before trusting them —
an earlier draft of this plan had them wrong.

They all follow one shape — e.g. `src/tools/memory_forget.rs:144-147`:

```rust
        let readonly = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
```

**Do not** try to keep those literals working. Instead add builder methods on
`SecurityPolicy` (in `src/security/policy.rs`, next to `apply_config`) so the
migration is a mechanical rewrite rather than a judgement call at 41 sites:

```rust
    /// Test/bootstrap helper: override one config field on an existing policy.
    /// Mutating the slot rather than the struct keeps `PolicyFields` private.
    #[must_use]
    pub fn with_autonomy(self, level: AutonomyLevel) -> Self { /* clone fields, set, store, return self */ }
    #[must_use]
    pub fn with_allowed_commands(self, cmds: Vec<String>) -> Self { … }
    #[must_use]
    pub fn with_forbidden_paths(self, paths: Vec<String>) -> Self { … }
    #[must_use]
    pub fn with_workspace_only(self, on: bool) -> Self { … }
    #[must_use]
    pub fn with_max_actions_per_hour(self, n: u32) -> Self { … }
```

**Six builders, not five.** `workspace_dir` stays a plain field on
`SecurityPolicy` (it is not moving into `PolicyFields`), but **19 of the 41
literals name it**, so it still needs a builder for the rewrite to be
mechanical. All 19 name it **alongside** a moved field; none names it alone:

```rust
    #[must_use]
    pub fn with_workspace_dir(mut self, dir: PathBuf) -> Self { self.workspace_dir = dir; self }
```

With that sixth builder the set covers every one of the six fields named at
those sites — verified by brace-matching all 41 literals. Then
rewrite each site:

```rust
        let readonly = Arc::new(SecurityPolicy::default().with_autonomy(AutonomyLevel::ReadOnly));
```

Work file by file (`cargo check --all-targets` after each) so a mistake is
attributable. Find them with:

```
grep -rln "SecurityPolicy {" src/ | grep -v '^src/security/'
```

**Verify**: `cargo check --all-targets` → no errors remain **outside**
`src/security/policy.rs`; that file's own test module stays broken until
Step 2c, so do not expect exit 0 here either.
`grep -rn "SecurityPolicy {" src/ | grep -v '^src/security/' | wc -l` returns
`0` (before: `41` on today's tree — expect **42** by the time this plan runs,
because plan 047's test adds one literal in `src/approval/mod.rs`. Re-count
rather than trusting the number; only the target of `0` is load-bearing).

### Step 2c: Migrate `src/security/policy.rs`'s own test module

Step 2b handled the 41 literals outside `src/security/`. The 35 literals **and**
21 field reads inside `policy.rs`'s own test module are still broken. No other
step owns them — this one does.

- 29 of the 35 literals name a moved field: rewrite each with the builders, same
  shape as Step 2b. (The other six — `:2126`, `:2179`, `:2206`, `:2238` name only
  `workspace_dir`; `:2291`, `:2304` only `policy_dir` — name no moved field.
  Being in a child module of `policy` they can still see private fields, so they
  compile unchanged. Leave them alone.)
- Convert the **21** field reads to `policy.fields().<field>`, per Step 1's list.
  Do not touch `:1573` — see Step 1.
- **Delete three tests that call the accessors Step 2 removed**, rather than
  repairing them: `set_autonomy_hot_swaps_across_clones` (`:1139`, calls
  `effective_autonomy()` at `:1151`),
  `effective_allowed_commands_falls_back_to_boot_list` (`:1196`, `:1198`), and
  `effective_autonomy_falls_back_to_boot_level` (`:1202`, `:1205`). Step 6's
  delete list already names all three — this pulls their deletion forward,
  because without it the tree cannot compile here, nor at Steps 3, 4 or 5.
  **Strike those three from Step 6's list when you reach it**; the other two
  (`set_allowed_commands_narrows_across_clones` `:1161`,
  `set_allowed_commands_leaves_operator_grants_intact` `:1184`) still belong to
  Step 6, since the slots they exercise survive until then.

  They cover boot-fallback behaviour that `apply_config` replaces wholesale, and
  Step 7's new tests are the replacement — so that behaviour is uncovered between
  here and Step 7. Acceptable inside one indivisible refactor; do not stop midway.

- **Convert five tests that write through the doomed override slots**, pulling
  this forward from Step 6. Step 2 made `first_unallowed_basename` read
  `fields().allowed_commands`, but `set_allowed_commands` — which writes
  `allowed_commands_runtime` — survives until Step 6, and `is_command_allowed`'s
  `None =>` arm (`:732`) still consults the override. So between Step 2 and
  Step 6 the two functions disagree, which is exactly the bug plan 048 exists to
  fix, and 048's own regression tests go red:

  | Test | Added by | Fix |
  |---|---|---|
  | `first_unallowed_basename_names_a_command_dropped_by_a_reload` | 048 | replace `set_allowed_commands(v)` with `apply_config(&cfg)` where `cfg.allowed_commands == v` |
  | `first_unallowed_basename_matches_is_command_allowed_after_narrowing` | 048 | same |
  | `first_unallowed_basename_still_honours_runtime_grants` | 048 | same (passes either way, but convert it for consistency) |
  | `needs_approval_follows_a_live_autonomy_tightening` | 047 | replace `set_autonomy(l)` with `apply_config(&cfg)`; `full_config()` / `supervised_config()` are already in `src/approval/mod.rs` |
  | `needs_approval_follows_a_live_autonomy_loosening` | 047 | same |

  The 047 pair lives in `src/approval/mod.rs` — the **third** permitted edit in
  that file, on top of the two named in Scope. Without this the 047 tests are
  silently red from Step 2 to Step 6 (no Verify between those steps runs
  `approval::`, so nothing catches them), and the 048 pair breaks the Verify
  directly below.

**One test stays red here, by design.**
`maybe_apply_runtime_config_update_applies_autonomy_when_provider_build_fails`
(`src/channels/mod.rs:5201`) asserts through `effective_autonomy()` at `:5263`
and `:5285`. Step 2 converts those reads to `fields().autonomy`, but the
production writer on that path is still `set_autonomy` → the dead
`autonomy_runtime` slot until **Step 3**. So the test fails at the end of this
step and goes green at the end of Step 3. None of this step's Verify commands
runs it (`--lib security` and `--lib approval` do not match `channels::`), but
if you run a full `cargo test --lib` here, that one failure is expected — do
not treat it as drift and do not try to fix it early.

This is the **first point in the plan where the tree compiles again**. Everything
from Step 1 through here is one indivisible refactor.

**Verify**: `cargo check --all-targets` → **exit 0** (the first `exit 0` since
Step 1); `cargo test --lib security` → all pass (this is what the five conversions
above buy you); `cargo test --lib approval` → all pass;
`grep -c "SecurityPolicy {" src/security/policy.rs` returns **`12`** (before:
`41`) — the struct declaration (`:87`), `impl Default` (`:134`),
`impl SecurityPolicy` (`:445`), the three `fn … -> SecurityPolicy {` signatures
(`:1093`, `:1097`, `:1104`), **and the six literals above that you deliberately
left in place**. Anything higher means literals remain. Anything *lower* means
you migrated one of the six — undo it: `:2291`/`:2304` name `policy_dir`, which
has no builder, and chasing a count of `6` would drop you into STOP condition 4
for no reason.

### Step 3: Switch the channels reload to `apply_config`

**Do NOT add a field to `ChannelRuntimeDefaults`.** It has three construction
sites (`:530`, `:569`, `:4975`), and the one at `:569`
(`runtime_defaults_snapshot`'s fallback) builds from `ChannelRuntimeContext`,
which carries no `AutonomyConfig` — you would have to invent a value.

Instead, get the config from where it is already parsed.
`load_runtime_defaults_from_config_file` (`:616`) already deserialises a full
`Config` at `:620`. Change its return type to
`Result<(ChannelRuntimeDefaults, crate::config::AutonomyConfig)>` and return
`parsed.autonomy` alongside. It has exactly **one** caller —
`maybe_apply_runtime_config_update` at `:653` — so this is a two-line change.

Then replace the two setter calls at `:671-676` with a single
`ctx.security.apply_config(&next_autonomy);`.

Remove the now-unused `allowed_commands` / `autonomy_level` courier fields
**only if** nothing else reads them — `grep -rn "next_defaults.allowed_commands\|next_defaults.autonomy_level\|\.autonomy_level\b" src/channels/mod.rs` first. If something does, leave them and note it.

**Verify**: `cargo check --all-targets` → exit 0; `cargo test --lib channels`
→ all pass, including the existing hot-reload test at `src/channels/mod.rs:5071`.

### Step 4: Give the cron scheduler a refresh point

In `src/cron/scheduler.rs`, the policy at `:27-30` is built before the loop at
`:37`. Inside the loop, before dispatching due jobs, re-read the config from
disk and call `security.apply_config(&cfg.autonomy)`.

`load_runtime_defaults_from_config_file` is private to `channels` and keyed on
that module's config path — do **not** reach for it. Use
`crate::config::Config::load_or_init()` and take `.autonomy` from the result;
that is what the CLI cron paths already do. Refresh **per poll tick**, not per
job, so the cost is one config read per interval (floored at 5s by
`MIN_POLL_SECONDS`).

If the config read fails, log a warning and keep the previous fields — never
fall back to a permissive default.

**Verify**: `cargo check --all-targets` → exit 0; `cargo test --lib cron` →
all pass. Then confirm the refresh is in **production** code, not only in the
test: `awk '/pub async fn run/,/^}/' src/cron/scheduler.rs | grep -c apply_config`
returns at least `1`. Test 6 calls `apply_config` directly, so a grep over the
whole file would be satisfied by the test's own text with no production change.

### Step 5: Simplify the gateway

`build_tools_factory` (`src/gateway/mod.rs:486`) currently builds a whole new
policy per turn. Make the policy long-lived and refresh it per turn instead.

**Do NOT hoist a `SecurityPolicy::default()` outside the closure.**
`build_tools_factory(runtime, mem)` receives no `Config` — the config only
arrives *inside* the closure — so a hoisted default would carry
`workspace_dir` from `Default`, not from the real config. `file_write` writes
to `self.security.workspace_dir.join(path)` (`src/tools/file_write.rs:86`) and
`is_resolved_path_allowed` (`src/security/policy.rs:880-886`) containment-checks
against the same field, so that would silently relocate the gateway's write
root. `apply_config` deliberately does not carry `workspace_dir`.

Hoist a lazily-initialised slot instead, so the first turn builds the policy
from the real config and later turns only refresh it:

```rust
    let policy: Arc<Mutex<Option<Arc<SecurityPolicy>>>> = Arc::new(Mutex::new(None));
    Arc::new(move |config: &Config| {
        let security = {
            let mut slot = policy.lock();
            match slot.as_ref() {
                Some(p) => { p.apply_config(&config.autonomy); Arc::clone(p) }
                None => {
                    let p = Arc::new(SecurityPolicy::from_config(
                        &config.autonomy, &config.workspace_dir));
                    *slot = Some(Arc::clone(&p));
                    p
                }
            }
        };
```

Note the consequence and leave it: `workspace_dir` is fixed at first use on
this path. Changing the workspace mid-process is not a supported operation and
is listed Out-of-scope.
The registry is still rebuilt per turn; the policy is refreshed rather than
replaced, so `tracker`, `runtime_allowlist`, and `pending` all survive.

Also delete plan 046's now-dead `let tracker = Arc::new(ActionTracker::new());`
hoist in `build_tools_factory` — the lazily-built policy owns the tracker now.

Delete `from_config_with_shared_tracker` (added by plan 046) **and plan 046's
test that calls it** (`shared_tracker_accumulates_across_rebuilt_policies` in
`src/security/policy.rs`). The constructor cannot be removed while that test
references it, and its guarantee is taken over by this plan's
`apply_config_keeps_the_action_budget`. Deleting it is expected, not a STOP.

**Verify**: `cargo check --all-targets` → exit 0. Plan 046's *remaining* two
tests still pass (`independent_policies_do_not_share_a_tracker` and
`tools_factory_shares_one_action_tracker_across_turns`); the third was deleted
above by design.

### Step 6: Delete the old slots

Remove `autonomy_runtime`, `allowed_commands_runtime`, `set_autonomy`,
`set_allowed_commands`, and every test that exercises them.

**Because 047 and 048 land first, there are more call sites than the five
originally in `policy.rs`. All of these are expected — none is a STOP:**

- `src/approval/mod.rs` — 047's tests 1 and 2 called `policy.set_autonomy(...)`.
  **Step 2c already converted them** to `apply_config`; expect no `set_autonomy`
  call left here. That was the **third** permitted edit in that file, in addition
  to the two named in Scope. If any remains, convert it now.
- `src/security/policy.rs` — 048's three `first_unallowed_basename_*` tests
  called `set_allowed_commands`. **Step 2c already converted them** to
  `apply_config`. **Keep the tests** — they cover a real bug. If any
  `set_allowed_commands` call survives here, convert it now.

The original `policy.rs` tests to **delete** outright. **Step 2c already removed
three of them** — expect them missing; that is not drift:
`set_autonomy_hot_swaps_across_clones` (`:1139`) — *gone in Step 2c*,
`effective_allowed_commands_falls_back_to_boot_list` (`:1196`) — *gone in Step 2c*,
`effective_autonomy_falls_back_to_boot_level` (`:1202`) — *gone in Step 2c*,
`set_allowed_commands_narrows_across_clones` (`:1161`) — delete here,
`set_allowed_commands_leaves_operator_grants_intact` (`:1184`) — delete here.

**Those five tests are replaced by the new ones in the Test plan — deleting
them is expected, not a STOP.** Write the replacements first (Step 7) so
coverage never drops to zero.

**Verify**: `cargo check --all-targets` → exit 0.

### Step 7: Tests, then full verification

Write the Test plan below, then:

- `cargo fmt --all -- --check` → exit 0
- `cargo clippy --locked --all-targets -- -D clippy::correctness` → exit 0
- `cargo test --lib` → exit 0

## Test plan

In `src/security/policy.rs` tests:

1. `apply_config_swaps_every_config_field` — build a policy, call
   `apply_config` with an `AutonomyConfig` differing in **all eight** fields,
   including `max_cost_per_day_cents` — it has no *production* reader at all
   (only test assertions at `src/security/policy.rs:1570` and `:1586`), so this
   test is its only behavioural coverage and is easy to drop by accident,
   assert each one changed. *This is the test that would have caught the
   five-frozen-field gap.*
2. `apply_config_is_visible_through_a_clone` — the replacement for
   `set_autonomy_hot_swaps_across_clones`: clone the policy, `apply_config` on
   one, assert the clone sees the new level and the new allowlist. Pins the
   inner-`Arc` propagation the channels tool registry depends on.
3. `apply_config_keeps_the_action_budget` — record actions, `apply_config`,
   assert the count carried over.
4. `apply_config_keeps_operator_granted_commands` — `add_runtime_command`,
   `apply_config` with a config not listing it, assert still allowed.

In `src/channels/mod.rs` tests:

5. `channels_reload_applies_forbidden_paths` — model on the existing test at
   `:5071`; assert a newly-added forbidden path is enforced after a reload.
   Fails on today's code.

In `src/cron/scheduler.rs` tests:

6. `cron_scheduler_applies_an_autonomy_change` — `apply_config` the scheduler's
   policy to `ReadOnly`, assert a job is refused. Fails on today's code.

**Mutation checks (required)**:

- Make `apply_config` copy one field from the old value instead of the new one
  → test 1 must fail, naming that field.
- Make `apply_config` replace the whole `SecurityPolicy` rather than the inner
  slot → test 2 must fail. *This is the exact regression an earlier draft of
  this plan contained; the test exists to make it impossible.*
- Make `apply_config` reset the tracker → test 3 must fail.

## Done criteria

Machine-checkable. Each command below was **run against the current tree** and
returns the "before" value shown, so each is genuinely falsifiable:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --locked --all-targets -- -D clippy::correctness` exits 0
- [ ] `cargo test --lib` exits 0
- [ ] `grep -rn "autonomy_runtime" src/ | wc -l` returns `0` (before: `7`)
- [ ] `grep -rn "allowed_commands_runtime" src/ | wc -l` returns `0` (before: `6`)
- [ ] `grep -rn "\.set_autonomy(\|\.set_allowed_commands(" src/ | wc -l` returns `0` (before: `7`; this form does **not** match the unrelated `fn set_autonomy` handler in `src/gateway/config_api.rs`)
- [ ] `grep -rn "from_config_with_shared_tracker" src/ | wc -l` returns `0`.
      (Vacuous against today's tree — the symbol only exists after 046 lands.
      Re-check it *after* 046, not before.)
- [ ] `grep -rn "fn apply_config" src/security/policy.rs | wc -l` returns `1` (before: `0`)
- [ ] `grep -rn "SecurityPolicy {" src/ | grep -v '^src/security/' | wc -l` returns `0` (before: `41` today, `42` once 047 lands — re-count rather than
      trusting either number; only the `0` target is load-bearing)
- [ ] `grep -rn "fn with_autonomy" src/security/policy.rs | wc -l` returns `1` (before: `0`)
- [ ] `grep -rn "fn with_workspace_dir" src/security/policy.rs | wc -l` returns `1` (before: `0`)
- [ ] All eight builders exist:
      `grep -cE "pub fn with_(autonomy|allowed_commands|forbidden_paths|workspace_only|max_actions_per_hour|workspace_dir|require_approval_for_medium_risk|block_high_risk_commands)\(" src/security/policy.rs`
      returns `8` (before: `0`)
- [ ] No dead builder was added:
      `grep -c "fn with_max_cost_per_day_cents" src/security/policy.rs` returns `0`.
      (Deliberately vacuous — `0` before and after. It is a negative guard
      against a builder an earlier draft of this plan wrongly called for,
      not a state-change check.)
- [ ] `grep -rn "shared_tracker_accumulates_across_rebuilt_policies" src/ | wc -l` returns `0` (046's test, superseded by this plan). Also vacuous until 046 lands.
- [ ] The scheduler's **poll loop** refreshes, not just its tests:
      `awk '/pub async fn run/,/^}/' src/cron/scheduler.rs | grep -c apply_config`
      returns at least `1` (before: `0`)
- [ ] The six new tests exist and pass; all three mutation checks were performed and each named test failed under its own mutation
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row updated with an 8-column row

## STOP conditions

Stop and report back (do not improvise) if:

- `src/security/policy.rs:98` reads `pub tracker: ActionTracker` with no
  `Arc` — plan 046 has not landed; this plan's process-state guarantee is
  invalid without it.
- A `fields()` snapshot would have to be held across an `.await`. Resolve to
  the owned `Arc<PolicyFields>` first; if a call site makes that impossible,
  report it — holding a `parking_lot` guard across an await can deadlock.
- Step 2 finds a **reader** (not a construction site) of a now-private field
  outside the in-scope files. The 41 construction sites in `src/tools/**` are
  expected and Step 2b handles them — their volume is **not** a STOP.
- Step 2b finds a construction site that names a field with no matching
  `with_*` builder. Add one rather than reaching back into `PolicyFields`
  directly, and note it in your report.
- Test 2 (`apply_config_is_visible_through_a_clone`) fails at any point. That
  means the refresh is not reaching clones, which is the whole mechanism —
  do not work around it.
- The channels hot-reload test at `src/channels/mod.rs:5071` fails after Step
  3 in a way that is not a rename.

## Maintenance notes

- The invariant to defend in review: **config state goes in `PolicyFields`;
  process state stays on `SecurityPolicy`.** Every field added to the policy
  needs that decision made explicitly. A field placed on the wrong side either
  fails to refresh (the five-frozen-field bug) or silently resets (the
  rate-limiter regression).
- Making the config fields private is load-bearing: it is what stops a future
  gate from reading a stale field directly, which is the bug plan 048 fixes and
  which review alone failed to prevent twice.
- Deliberately deferred: `workspace_dir` is still a plain field; the ~72 tool
  handles are untouched; `src/agent/**` still builds its own policy per Agent.
  None of those block this plan, and each is smaller once this lands.
