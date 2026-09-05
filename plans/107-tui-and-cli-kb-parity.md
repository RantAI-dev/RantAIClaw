# Plan 107: TUI `/kb` and CLI `kb enable`/`disable`, plus the provisioner category fix

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/tui/commands/ src/kb/axi/cli.rs src/onboard/provision/knowledge.rs src/main.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: LOW
- **Depends on**: 102, 104
- **Category**: feature
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

After the console gains an activation toggle, the TUI and CLI have no
equivalent — and the TUI has no KB status surface at all.

**TUI.** The command registry (`src/tui/commands/mod.rs:168-205`) holds 34
commands; none is KB. The only KB surface is `/setup knowledge`, which runs the
provisioner once and then offers nothing: no way to see whether the KB is on,
whether a key resolves, or to turn it off.

**CLI.** `KbCommand` has nine subcommands — `search, ingest, list, get, delete,
drift, re-embed, intelligence, graph` (`src/kb/axi/cli.rs:46-147`). None reports
status or toggles the feature, so headless setup cannot activate the KB.

**A category bug found while reading this.** `KnowledgeProvisioner` does not
override `fn category()` — `grep -c 'fn category' src/onboard/provision/knowledge.rs`
returns 0 — so it falls back to `ProvisionerCategory::Core`
(`provision/traits.rs:76-78`). But the first-run wizard registers it under
Integrations (`tui/first_run_wizard.rs:1187-1190`). The same thing appears in a
different group depending on how you reach it.

## Current state (verified at 2ca7e59)

- `KbCommand::run(embedding_api_key, vision_api_key)` — `cli.rs:154-158`. It
  receives **two key strings, not `Config`**, so an `enabled` check needs the
  signature widened. Caller: `src/main.rs:2235-2240`.
- `open_store` — `cli.rs:765-769`
- TUI provisioner entry point: `/setup knowledge` → `provisioner_for("knowledge")`
  (`onboard/provision/registry.rs:14`)

## Scope

**In scope**: `/kb` in the TUI, `kb status|enable|disable` in the CLI, the
category override.

**Out of scope**: a full KB browser in the TUI (list/search/graph). Status and
toggle only — that is what parity with the console's activation screen requires.
Note the larger surface as a follow-up rather than growing this plan.

## Git workflow

```bash
git switch -c feat/kb-tui-cli-parity
```

## Steps

### Step 1: Widen `KbCommand::run` to take the config

```rust
    pub async fn run(self, config: &crate::config::Config) -> KbResult<i32> {
        let cfg = KbConfig::from_env_with_keys(
            config.knowledge.embedding_api_key.as_deref(),
            config.knowledge.vision_api_key.as_deref(),
        )?;
```

Update `main.rs:2235-2240`. Passing the whole config also removes the awkward
two-argument call site.

**Verify**: `cargo build --features kb`.

### Step 2: Add `status`, `enable`, `disable`

```rust
    /// Show whether the Knowledge Base is active and whether a key resolves.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Activate the Knowledge Base.
    Enable,
    /// Deactivate the Knowledge Base. Credentials are kept.
    Disable,
```

`Status` emits TOON like every other read command (`enabled`,
`embedding_configured`, `vision_configured`, `source`, `db_path`,
`document_count`). `Enable`/`Disable` mutate `config.knowledge.enabled` and
persist.

Two things to get right:

- **They write config**, unlike every other `KbCommand`. Take the same care the
  gateway does: load fresh from disk, mutate, save — do not persist a stale
  in-memory snapshot.
- `Enable` with no resolvable key must refuse with a clear message rather than
  producing a KB that reports enabled and then 503s.

Skip `open_store` for these three — `Status` may report `db_path` without
opening (opening rewrites `kb_meta`, see plan 098), and the toggles do not need
the store at all.

**Verify**: `rantaiclaw kb status` before and after `kb enable`.

### Step 3: Gate the other subcommands

In `run`, before dispatch, return an operational error for the data subcommands
when disabled:

```rust
        if !config.knowledge.enabled
            && !matches!(self, Self::Status { .. } | Self::Enable | Self::Disable)
        {
            print_error_toon(
                "kb_disabled",
                "Knowledge Base is off. Run `rantaiclaw kb enable`.",
            );
            return Ok(1);
        }
```

Exit code 1 with a TOON error block matches the AXI contract (`axi/mod.rs:8-10`)
and gives the agent a parseable signal instead of a confusing empty result.

### Step 4: TUI `/kb`

Add `src/tui/commands/kb.rs` implementing `CommandHandler`, registered in
`mod.rs:register_defaults`. Follow `config::StatusCommand` for the shape.

- `/kb` → overlay with status: enabled, key source, db path, document count,
  and the hint that `/setup knowledge` configures it
- `/kb enable` / `/kb disable` → toggle, persist, confirm with a message

Feature-gate the whole module on `kb` so a non-KB build does not register a
command that cannot work.

**Verify**: `cargo test --features kb tui::commands`.

### Step 5: Fix the provisioner category

```rust
    fn category(&self) -> ProvisionerCategory {
        ProvisionerCategory::Integration
    }
```

Add a test asserting the category matches the first-run wizard's grouping, so
the two cannot drift again.

### Step 6: Docs

`docs/reference/commands.md` — the three new subcommands and `/kb`.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb
```

End-to-end:

```bash
cargo build --release --features kb
./target/release/rantaiclaw kb status
./target/release/rantaiclaw kb list          # expect kb_disabled, exit 1
echo $?
./target/release/rantaiclaw kb enable        # expect refusal if no key
./target/release/rantaiclaw kb status
```

TUI, in tmux: `/kb`, `/kb disable`, `/kb`, `/kb enable`.

## Done criteria

- `kb status|enable|disable` work and persist.
- Data subcommands report `kb_disabled` with exit 1 when off.
- `/kb` shows status and toggles.
- The provisioner appears in the same group everywhere, with a test.

## STOP conditions

- Widening `KbCommand::run` touches callers beyond `main.rs:2235` — report them.
- `Enable` can be made to persist a config that the gateway then rejects — the
  two must agree on what "enabled with no key" means; align with plan 103.
