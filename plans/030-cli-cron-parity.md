# Plan 030: CLI cron parity — agent-job create, `cron run`, `cron runs`

> **Context**: The parity goal (all surfaces equally capable, presented to fit the
> medium) leaves two gaps in the CLI: it can't create **agent** jobs (shell only)
> and has no **run-now** or **run-history** subcommand — both of which the web
> (027/028), TUI (029), and agent-tools already have. This plan closes them so
> `rantaiclaw cron` reaches feature parity, kept CLI-simple (short subcommands +
> flags, text output).
>
> **Executor note**: Verify — `cargo fmt --all -- --check` · scoped
> `cargo clippy -- -D warnings` · `cargo test --lib cron`. **CRITICAL two-crate-root
> gotcha** (see below): `CronCommands` is defined TWICE — `src/lib.rs:259`
> (`pub(crate)`, `Serialize/Deserialize/Clone/PartialEq/Eq` derives) AND
> `src/main.rs:973` (`Subcommand, Debug`). `src/cron/mod.rs` is compiled into BOTH
> crate roots and its `handle_command` `match` must stay exhaustive for each, so
> **every variant/field you add goes in BOTH enums, identically-shaped**. `cargo
> build --lib` only checks the lib crate — you MUST also `cargo build` (or `clippy
> --all-targets`) to compile the binary crate root and catch a missed edit.
>
> **Depends on**: plan 026 Task 5 (`cron::scheduler::run_job_manual`, for `cron
> run`). **Branch**: `feat/cli-cron-parity`. **Risk**: LOW–MED (CLI surface; no
> schema/exposure change).

## Baseline evidence (confirmed against main, 2026-07-19)

- **Two crate roots**: `src/main.rs:60` `mod cron;` compiles `src/cron/mod.rs` into
  the binary crate; `src/lib.rs` compiles it into the library crate. Each binds
  `crate::CronCommands` to its own enum (`lib.rs:259` / `main.rs:973`). Dispatch:
  `main.rs:1821` `Some(Commands::Cron { cron_command }) => cron::handle_command(cron_command, &config)`.
- **`CronCommands` today** (both copies, live order): `List`, `Add { expression, tz,
  command }`, `AddAt { at, command }`, `AddEvery { every_ms, command }`,
  `Once { delay, command }`, `Remove { id }`, `Update { id, expression, tz, command,
  name }`, `Pause { id }`, `Resume { id }`. `handle_command` (`src/cron/mod.rs:22-164`)
  matches all of them; every `Add*`/`Once` arm calls `add_shell_job` (shell only).
  New `Run`/`Runs` variants go after `Resume` (order within the enum is cosmetic).
- **`main` is `async fn`** (`main.rs:1296`, `#[tokio::main]` → multi-thread runtime),
  but `handle_command` is **sync** (`cron/mod.rs:23`). Store fns are sync; the only
  async new op is `run_job_manual`. Pattern for driving async from the sync handler:
  `tokio::task::block_in_place(|| Handle::current().block_on(fut))` (multi-thread
  runtime supports it) — same bridge the TUI uses.
- **Store/scheduler fns**: `add_agent_job(&Config, Option<String>, Schedule, &str,
  SessionTarget, Option<String>, Option<DeliveryConfig>, bool)`,
  `list_runs(&Config,&str,usize) -> Vec<CronRun>`, `get_job`,
  `cron::scheduler::run_job_manual(&Config,&CronJob) -> (bool,String)` (026).
  `CronRun { id, job_id, started_at, finished_at, status, output, duration_ms }`.
  `SessionTarget::Isolated`.

## Scope
- **In**: `src/lib.rs` + `src/main.rs` (both `CronCommands` enums — add `--agent`/
  `--model` to `Add`/`AddAt`/`AddEvery`/`Once`; add `Run`/`Runs` variants),
  `src/cron/mod.rs` (`handle_command` arms + a testable async run helper),
  `docs/reference/commands.md` (document the new flags/subcommands).
- **Out**: `src/cron/store.rs`/`scheduler.rs` (consumed as-is), any schema/exposure
  change, the TUI/web (029/028).

**Parity added**: `rantaiclaw cron add --agent '<expr>' '<prompt>' [--model <m>]`
(+ same on `add-at`/`add-every`/`once`), `rantaiclaw cron run <id>`,
`rantaiclaw cron runs <id> [--limit N]`.

---

## Task 1 — Agent-job creation flag on the `add*` subcommands

**Files:** `src/lib.rs`, `src/main.rs` (both `CronCommands`), `src/cron/mod.rs`.

- [ ] **Step 1 — Failing test** in `src/cron/mod.rs` `tests` (drives `handle_command`
  like the existing update tests at `mod.rs:272-415`):

```rust
    #[test]
    fn add_agent_job_via_handler() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        handle_command(
            crate::CronCommands::Add {
                expression: "0 9 * * *".into(),
                tz: None,
                command: "Summarize overnight emails".into(),
                agent: true,
                model: Some("claude-opus-4-8".into()),
            },
            &config,
        )
        .unwrap();
        let job = &list_jobs(&config).unwrap()[0];
        assert_eq!(job.job_type, JobType::Agent);
        assert_eq!(job.prompt.as_deref(), Some("Summarize overnight emails"));
        assert_eq!(job.model.as_deref(), Some("claude-opus-4-8"));
        assert!(job.command.is_empty());
    }
```

  (Add `use crate::cron::JobType;`/`list_jobs` as needed — mirror the module's
  existing test imports.)

- [ ] **Step 2 — Run, confirm FAIL** (the `Add` variant has no `agent`/`model` fields).

- [ ] **Step 3 — Add the fields to BOTH enums.** In `src/lib.rs` `Add` variant
  (after `command`):

```rust
        /// Create an AGENT job (the positional is the prompt, not a shell command).
        #[arg(long)]
        agent: bool,
        /// Model override for an agent job (ignored for shell jobs).
        #[arg(long)]
        model: Option<String>,
```

  Add the SAME two fields to `AddAt`, `AddEvery`, and `Once` in `src/lib.rs`, and
  then add all of them **identically** to the mirror variants in `src/main.rs:973+`.
  (Keep each file's existing per-enum derives; only the variant shapes must match.)

- [ ] **Step 4 — Branch each `add*` arm in `handle_command`** (`src/cron/mod.rs`).
  Replace the `Add` arm (`mod.rs:57-72`) — and analogously `AddAt`/`AddEvery`/`Once`:

```rust
        crate::CronCommands::Add { expression, tz, command, agent, model } => {
            let schedule = Schedule::Cron { expr: expression, tz };
            let job = add_scheduled(config, schedule, &command, agent, model, false)?;
            print_added(&job);
            Ok(())
        }
```

  Add a shared helper (near `add_once`):

```rust
    /// Create a shell or agent job from a resolved schedule. `delete_after_run`
    /// applies to agent one-shots (`At`); shell jobs ignore it (store limitation).
    fn add_scheduled(
        config: &Config,
        schedule: Schedule,
        payload: &str,
        agent: bool,
        model: Option<String>,
        delete_after_run: bool,
    ) -> Result<CronJob> {
        if agent {
            add_agent_job(
                config, None, schedule, payload,
                crate::cron::SessionTarget::Isolated, model, None, delete_after_run,
            )
        } else {
            add_shell_job(config, None, schedule, payload)
        }
    }

    fn print_added(job: &CronJob) {
        println!("✅ Added cron job {}", job.id);
        // Schedule-aware line — `job.expression` is "" for At/Every, so don't
        // print an empty `Expr:` (that regressed the bespoke AddAt/AddEvery/Once
        // output which printed `At:`/`Every(ms):`).
        match &job.schedule {
            Schedule::Cron { expr, .. } => println!("  Expr: {expr}"),
            Schedule::At { at } => println!("  At  : {}", at.to_rfc3339()),
            Schedule::Every { every_ms } => println!("  Every(ms): {every_ms}"),
        }
        println!("  Next: {}", job.next_run.to_rfc3339());
    }
```

  For `AddAt`/`Once` (one-shot `At`), pass `delete_after_run = agent` (agent
  one-shots auto-delete on success, matching the `cron_add` tool; shell one-shots
  stay — plan 026 fixes their re-fire by disabling). For `AddEvery`, pass `false`.
  Import `crate::cron::CronJob`/`add_agent_job` at the top of `mod.rs` if not
  already in scope (`add_agent_job` is re-exported via `cron/mod.rs:16-19`; `CronJob`
  via `types`).

- [ ] **Step 5 — Run + build BOTH crate roots.**
  `cargo test --lib cron::tests::add_agent_job_via_handler` then
  `cargo build` (compiles the binary crate root too — proves both `CronCommands`
  enums stayed in sync). Expected: PASS + clean build.

- [ ] **Step 6 — Commit.** `git commit -m "feat(cli): let cron add/add-at/add-every/once create agent jobs (--agent --model)"`

---

## Task 2 — `cron run <id>` (force-run) and `cron runs <id>` (history)

**Files:** `src/lib.rs`, `src/main.rs` (both `CronCommands`), `src/cron/mod.rs`.

- [ ] **Step 1 — Failing tests** in `src/cron/mod.rs` `tests`:

```rust
    #[tokio::test]
    async fn cli_run_reports_and_records() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_shell_job(&config, None, Schedule::Cron { expr: "*/5 * * * *".into(), tz: None }, "echo cli-run").unwrap();
        let out = run_job_report(&config, &job.id).await.unwrap();
        assert!(out.contains("ok") || out.contains("cli-run"), "{out}");
        assert_eq!(list_runs(&config, &job.id, 10).unwrap().len(), 1);
    }

    #[test]
    fn cli_runs_lists_history() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_shell_job(&config, None, Schedule::Cron { expr: "*/5 * * * *".into(), tz: None }, "echo x").unwrap();
        record_run(&config, &job.id, Utc::now(), Utc::now(), "ok", Some("x"), 3).unwrap();
        let text = runs_report(&config, &job.id, 10).unwrap();
        assert!(text.contains(&job.id) || text.contains("ok"), "{text}");
    }
```

  **Required import:** add `use chrono::Utc;` to the `tests` module — `mod.rs`
  uses fully-qualified `chrono::Utc` everywhere and has NO `use chrono::Utc;`, so
  `use super::*;` does not bring `Utc` into scope and `Utc::now()` above will not
  compile without it. (`test_config`, `add_shell_job`, `list_jobs`, `list_runs`,
  `record_run`, `get_job`, `JobType`, `Schedule` are already in scope via the
  module's `pub use` at `mod.rs:16-20` — no extra imports needed for those.)

- [ ] **Step 2 — Run, confirm FAIL** (`run_job_report`/`runs_report` undefined; the
  `Run`/`Runs` variants don't exist).

- [ ] **Step 3 — Add the variants to BOTH enums.** In `src/lib.rs` `CronCommands`
  (after `Resume`) and identically in `src/main.rs`:

```rust
    /// Force-run a scheduled task now and record the run.
    Run {
        /// Job id
        id: String,
    },
    /// Show recent run history for a scheduled task.
    Runs {
        /// Job id
        id: String,
        /// Max rows to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
```

- [ ] **Step 4 — Implement the helpers + arms** in `src/cron/mod.rs`:

```rust
    /// Force-run a job and return a human report. Reuses the shared manual-run
    /// path (records run history; does not reschedule/deliver) — same semantics as
    /// the `cron_run` tool and the web `POST /cron/{id}/run`. NOTE: keep this
    /// NON-`pub` (only `handle_command` + the in-module test call it); a `pub` fn
    /// returning `Result` would trip `clippy::missing_errors_doc` under the
    /// post-merge pedantic strict-clippy-delta gate that `-D warnings` locally
    /// won't surface.
    async fn run_job_report(config: &Config, id: &str) -> Result<String> {
        let job = get_job(config, id)?;
        let (ok, output) = crate::cron::scheduler::run_job_manual(config, &job).await;
        Ok(format!(
            "{} cron job {id} ({})\n{}",
            if ok { "✅" } else { "✗" },
            if ok { "ok" } else { "error" },
            output.trim(),
        ))
    }

    fn runs_report(config: &Config, id: &str, limit: usize) -> Result<String> {
        let runs = list_runs(config, id, limit)?;
        if runs.is_empty() {
            return Ok(format!("No run history for cron job {id}."));
        }
        let mut out = format!("Run history for {id} ({}):\n", runs.len());
        for r in runs {
            out.push_str(&format!(
                "  {} · {} · {}ms\n",
                r.started_at.to_rfc3339(),
                r.status,
                r.duration_ms.unwrap_or(0),
            ));
        }
        Ok(out)
    }
```

  Add the `handle_command` arms (after `Resume`, `mod.rs:158-162`):

```rust
        crate::CronCommands::Run { id } => {
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|_| anyhow::anyhow!("cron run must execute inside the tokio runtime"))?;
            let report = tokio::task::block_in_place(|| handle.block_on(run_job_report(config, &id)))?;
            println!("{report}");
            Ok(())
        }
        crate::CronCommands::Runs { id, limit } => {
            println!("{}", runs_report(config, &id, limit)?);
            Ok(())
        }
```

- [ ] **Step 5 — Run + build BOTH roots.**
  `cargo test --lib cron` then `cargo build`. Expected: PASS + clean.

- [ ] **Step 6 — Docs.** In `docs/reference/commands.md`, under the cron section,
  add `cron run <id>`, `cron runs <id> [--limit]`, and the `--agent`/`--model`
  flags with one example each. Keep it short.

- [ ] **Step 7 — Commit.** `git commit -m "feat(cli): add cron run (force-run) and cron runs (history) subcommands"`

---

## Done criteria
- [ ] `cron add --agent '<expr>' '<prompt>' [--model]` (and `add-at`/`add-every`/
  `once`) create agent jobs; `add_agent_job_via_handler` test passes.
- [ ] `cron run <id>` force-runs + records history (reuses `run_job_manual`);
  `cron runs <id>` prints history. Both tests pass.
- [ ] BOTH `CronCommands` enums (lib.rs + main.rs) carry the new fields/variants,
  identically shaped; `handle_command` match is exhaustive in both crate roots.
- [ ] `cargo build` (binary root) AND `cargo test --lib cron` green; fmt + clippy clean.
- [ ] `docs/reference/commands.md` updated. No schema/exposure change.

## STOP conditions
- If `cargo build --lib` passes but `cargo build` fails with a non-exhaustive
  `match` or unknown-variant error, you edited only ONE `CronCommands` — sync the
  other crate root's enum. This is the expected failure if a variant is missed.
- If `run_job_manual` (026) is absent, `run_job_report` won't compile — land 026
  Task 5 first. **Do NOT "fix" the missing symbol by pointing at the existing
  `execute_job_now` (scheduler.rs:51)** — it has the IDENTICAL signature
  `(&Config, &CronJob) -> (bool, String)` and will compile cleanly, but it does
  NOT record a run (only `persist_job_result`, reached via the scheduled path,
  calls `record_run`). Substituting it makes `cli_run_reports_and_records` fail
  (`list_runs(...).len()` sees 0). `run_job_manual` (026 Task 5) is the version
  that records; if 026 slips, reproduce it inline (`execute_job_now` + `record_run`
  + `record_last_run`), don't just rename.
- If `#[arg(long)] agent: bool` collides with clap's handling of a bare flag on a
  variant that also has two positionals (`expression`, `command`), verify the help
  parses (`rantaiclaw cron add --help`); reorder so the flag precedes/positions
  cleanly if clap complains.

## Rollback
Per-commit revert. All additive (new enum fields/variants + arms + helpers).
Reverting restores the shell-only, run-less CLI. No persisted-state/schema change.
