# Plan 029: TUI cron — full-parity `/cron` (browse + action-keys + agent/shell create + edit) and a `/doctor` scheduler line

> **Context**: The TUI `/cron` command is an advertised STUB
> (`src/tui/commands/cron.rs` — returns `"Integration with cron scheduler
> pending."`, registered in `/help` + the `/` autocomplete). This plan wires it to
> the real `crate::cron` store and gives the TUI **feature parity** with the web
> console (create shell AND agent jobs, edit, pause/resume, run-now, delete, browse
> + run history) — presented the TUI-simple way, not as web-style forms. Interaction
> model (chosen): a **hybrid** — an interactive jobs picker whose detail panel takes
> quick **action-keys** (`[r]un [p]ause/resume [d]elete`), plus concise subcommands
> for the multi-field operations (`/cron add [--agent] …`, `/cron edit <id> <field>
> <value>`).
>
> **Executor note**: Verify — `cargo fmt --all -- --check` · scoped
> `cargo clippy -- -D warnings` on changed files · `cargo test --lib
> tui::commands::cron tui::widgets::info_panel`. `app.rs` is ~347KB / high-churn —
> keep edits minimal + localized; rebase before merge. Pure store logic lives in
> `&Config`-taking helpers so unit tests avoid `block_in_place`/tokio.
>
> **Depends on**: plan 026 Task 5 (`cron::scheduler::run_job_manual`, for run-now).
> Land/cherry-pick 026 first. **Branch**: `feat/tui-cron`. **Risk**: MED (TUI +
> `app.rs`; no schema/exposure change).

## Baseline evidence (confirmed against main, 2026-07-19)

- **Stub + advertised**: `src/tui/commands/cron.rs` (whole file, never calls
  `crate::cron`); registered `commands/mod.rs:7,155`; enumerated into `/help` +
  autocomplete (`mod.rs:222-324`, `core.rs:45-61`).
- **No cron view**: `ListPickerKind` (`widgets/list_picker.rs:28-45`) has no `Cron`.
  A view is net-new; primitives are `ListPicker` + `InfoPanel`.
- **`ListPicker` API**: `ListPickerItem { key, primary, secondary }`;
  `ListPicker::new(kind, title, items: Vec<ListPickerItem>, preselect: Option<&str>, empty_msg)`
  → `CommandResult::OpenListPicker`; selection → `app.rs::dispatch_list_picker_selection`
  (`app.rs:2687`, `match kind` on `item.key`) — mirror `autonomy.rs:53-77`.
- **`InfoPanel` API** (`widgets/info_panel.rs:210-262`): fields
  `title, subtitle, sections, footer_hint, scroll_offset`; builder `new(title)`,
  `with_subtitle`, `with_footer`, `section(InfoSection)`; `scroll_up/down`,
  `page_up/down`. `InfoSection::new(t).status_with(StatusKind::{Ok|Warn|Fail|Info},
  label, value)` (`config.rs:160-299`). Opened via `CommandResult::OpenInfoPanel`
  or `self.info_panel = Some(panel)` (`app.rs:2615`).
- **info_panel key block** (`app.rs:964-998`): Up/Down/PageUp/PageDown scroll, Esc
  closes, then a catch-all `_ if self.info_panel.is_some()` (line 995) swallows
  every other key. Action-keys go in as arms **before** line 995.
- **Config bridge from sync `execute()`**: `block_in_place` + `Config::load_or_init().await`
  (`autonomy.rs:130-140`). `TuiContext` exposes no `Config`.
- **Store/scheduler fns** (`crate::cron`, all `pub`): `list_jobs`, `get_job`,
  `add_shell_job(&Config, Option<String>, Schedule, &str)`,
  `add_agent_job(&Config, Option<String>, Schedule, &str, SessionTarget, Option<String>, Option<DeliveryConfig>, bool)`,
  `update_job(&Config,&str,CronJobPatch)`, `remove_job`, `pause_job`, `resume_job`,
  `list_runs(&Config,&str,usize)`, `cron::scheduler::run_job_manual(&Config,&CronJob)`
  (026). `Schedule::Cron{expr,tz}`; `CronJobPatch` (`types.rs:136-147`);
  `SessionTarget::Isolated`.
- **Doctor is cross-process**: scheduler health is in the daemon process; read
  `crate::daemon::state_file_path(config)` → `daemon_state.json` (CLI doctor does
  this at `doctor/legacy.rs:597-685`), NOT `crate::health::snapshot()`.

## Scope
- **In**: `src/tui/commands/cron.rs` (rewrite), `src/tui/widgets/list_picker.rs`
  (`Cron` variant), `src/tui/widgets/info_panel.rs` (`cron_job_id` field +
  `with_cron_actions`), `src/tui/app.rs` (dispatch arm + 3 action-key arms + 3
  helper methods + `build_cron_detail_panel`), `src/tui/commands/config.rs`
  (doctor scheduler line).
- **Out**: `src/cron/*` (consumed as-is). No schema/exposure change.

**Parity target (all reachable from the TUI after this plan):** create shell +
agent · list · detail + run history · edit (schedule/name/cmd/prompt/model) ·
pause/resume · run-now · delete. Simple presentation: ONE picker + action-keys +
short subcommands (no web-style forms).

---

## Task 1 — `/cron` subcommands → store (shell+agent create, edit, remove/pause/resume)

**Files:** `src/tui/commands/cron.rs` (rewrite stub + tests).

- [ ] **Step 1 — Failing tests** (replace the 6 stub tests) against a pure
  `run_cron_text(&Config, &str) -> String` helper with a temp-workspace `Config`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use tempfile::TempDir;

    fn cfg(tmp: &TempDir) -> Config {
        let c = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&c.workspace_dir).unwrap();
        c
    }
    fn first_id(c: &Config) -> String {
        crate::cron::list_jobs(c).unwrap()[0].id.clone()
    }

    #[test]
    fn list_empty() {
        let tmp = TempDir::new().unwrap();
        assert!(run_cron_text(&cfg(&tmp), "list").contains("No cron jobs"));
    }
    #[test]
    fn add_shell_then_list() {
        let tmp = TempDir::new().unwrap();
        let c = cfg(&tmp);
        assert!(run_cron_text(&c, "add */5 * * * * echo hi").contains("Added"));
        let l = run_cron_text(&c, "list");
        assert!(l.contains("*/5 * * * *") && l.contains("echo hi"), "{l}");
    }
    #[test]
    fn add_agent_job() {
        let tmp = TempDir::new().unwrap();
        let c = cfg(&tmp);
        assert!(run_cron_text(&c, "add --agent 0 9 * * * Summarize emails").contains("Added"));
        let job = &crate::cron::list_jobs(&c).unwrap()[0];
        assert_eq!(job.job_type, crate::cron::JobType::Agent);
        assert_eq!(job.prompt.as_deref(), Some("Summarize emails"));
    }
    #[test]
    fn edit_reschedules_and_renames() {
        let tmp = TempDir::new().unwrap();
        let c = cfg(&tmp);
        run_cron_text(&c, "add */5 * * * * echo hi");
        let id = first_id(&c);
        assert!(run_cron_text(&c, &format!("edit {id} expr 0 8 * * *")).contains("Updated"));
        assert_eq!(crate::cron::get_job(&c, &id).unwrap().expression, "0 8 * * *");
        run_cron_text(&c, &format!("edit {id} name morning"));
        assert_eq!(crate::cron::get_job(&c, &id).unwrap().name.as_deref(), Some("morning"));
    }
    #[test]
    fn pause_resume_remove() {
        let tmp = TempDir::new().unwrap();
        let c = cfg(&tmp);
        run_cron_text(&c, "add */5 * * * * echo hi");
        let id = first_id(&c);
        run_cron_text(&c, &format!("pause {id}"));
        assert!(!crate::cron::get_job(&c, &id).unwrap().enabled);
        run_cron_text(&c, &format!("resume {id}"));
        assert!(crate::cron::get_job(&c, &id).unwrap().enabled);
        assert!(run_cron_text(&c, &format!("remove {id}")).contains("Removed"));
        assert!(crate::cron::list_jobs(&c).unwrap().is_empty());
    }
    #[test]
    fn unknown_subcommand() {
        let tmp = TempDir::new().unwrap();
        assert!(run_cron_text(&cfg(&tmp), "frobnicate").contains("Unknown"));
    }
}
```

- [ ] **Step 2 — Run, confirm FAIL** (`run_cron_text` undefined).

- [ ] **Step 3 — Rewrite `cron.rs`.** Full command + helpers:

```rust
use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::config::Config;
use crate::cron::{self, CronJobPatch, Schedule, SessionTarget};
use crate::tui::context::TuiContext;

pub struct CronCommand;

impl CommandHandler for CronCommand {
    fn name(&self) -> &str { "cron" }
    fn description(&self) -> &str { "Manage scheduled tasks" }
    fn usage(&self) -> &str {
        "/cron [list | add [--agent] <5-field-expr> <cmd|prompt> | edit <id> <field> <value> | remove|pause|resume <id>]"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let config = match load_config_blocking() {
            Ok(c) => c,
            Err(e) => return Ok(CommandResult::Message(format!("✗ cron: {e}"))),
        };
        // No arg / `list` → the interactive picker (Task 2). Everything else → text.
        let sub = args.split_whitespace().next().unwrap_or("");
        if sub.is_empty() || sub == "list" {
            return Ok(CommandResult::OpenListPicker(build_cron_picker(&config)));
        }
        Ok(CommandResult::Message(run_cron_text(&config, args)))
    }
}

fn load_config_blocking() -> Result<Config> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| anyhow::anyhow!("/cron must run inside a tokio runtime"))?;
    tokio::task::block_in_place(|| handle.block_on(async { Config::load_or_init().await }))
}

/// Pure store logic (unit-tested). Returns the message text.
fn run_cron_text(config: &Config, args: &str) -> String {
    let parts: Vec<&str> = args.split_whitespace().collect();
    match parts.first().copied().unwrap_or("list") {
        "list" | "" => list_text(config),
        "add" => add_text(config, &parts[1..]),
        "edit" => edit_text(config, &parts[1..]),
        "remove" => id_op(&parts, |id| cron::remove_job(config, id).map(|()| format!("🗑 Removed cron job {id}")), "remove"),
        "pause" => id_op(&parts, |id| cron::pause_job(config, id).map(|_| format!("⏸ Paused cron job {id}")), "pause"),
        "resume" => id_op(&parts, |id| cron::resume_job(config, id).map(|_| format!("▶ Resumed cron job {id}")), "resume"),
        other => format!("Unknown cron subcommand: {other}\n\nUsage: /cron [list|add|edit|remove|pause|resume]"),
    }
}

fn id_op(parts: &[&str], f: impl FnOnce(&str) -> anyhow::Result<String>, name: &str) -> String {
    match parts.get(1) {
        None => format!("Usage: /cron {name} <id>"),
        Some(id) => f(id).unwrap_or_else(|e| format!("✗ {e}")),
    }
}

fn list_text(config: &Config) -> String {
    match cron::list_jobs(config) {
        Ok(jobs) if jobs.is_empty() =>
            "Scheduled tasks:\n  No cron jobs configured.\n\nUse /cron add <5-field-expr> <cmd> to create one.".to_string(),
        Ok(jobs) => {
            let mut out = format!("Scheduled tasks ({}):\n", jobs.len());
            for j in jobs {
                let name = j.name.clone().unwrap_or_else(|| j.id[..j.id.len().min(8)].to_string());
                let what = if j.command.is_empty() { j.prompt.clone().unwrap_or_default() } else { j.command.clone() };
                out.push_str(&format!(
                    "  {} [{}] {} · next {} · {}\n    {}\n",
                    name, if j.enabled { "on" } else { "paused" }, j.expression,
                    j.next_run.to_rfc3339(), j.last_status.as_deref().unwrap_or("never run"), what,
                ));
            }
            out
        }
        Err(e) => format!("✗ Failed to list cron jobs: {e}"),
    }
}

/// `/cron add [--agent] <m> <h> <dom> <mon> <dow> <cmd-or-prompt...> [--model <m>]`
fn add_text(config: &Config, args: &[&str]) -> String {
    let is_agent = args.first() == Some(&"--agent");
    let rest = if is_agent { &args[1..] } else { args };
    // Optional trailing `--model <name>` (agent only).
    let (rest, model) = extract_flag(rest, "--model");
    if rest.len() < 6 {
        return "Usage: /cron add [--agent] <5-field-expr> <cmd-or-prompt> [--model <name>]\n  e.g. /cron add 0 9 * * * echo hi\n       /cron add --agent 0 9 * * * Summarize emails --model claude-opus-4-8".to_string();
    }
    let expr = rest[0..5].join(" ");
    let payload = rest[5..].join(" ");
    let schedule = Schedule::Cron { expr, tz: None };
    let result = if is_agent {
        cron::add_agent_job(config, None, schedule, &payload, SessionTarget::Isolated, model, None, false)
    } else {
        cron::add_shell_job(config, None, schedule, &payload)
    };
    match result {
        Ok(job) => format!("✅ Added cron job {}\n  Expr: {}\n  Next: {}", job.id, job.expression, job.next_run.to_rfc3339()),
        Err(e) => format!("✗ Failed to add cron job: {e}"),
    }
}

/// `/cron edit <id> <field> <value...>` — field ∈ expr|name|cmd|prompt|model.
fn edit_text(config: &Config, args: &[&str]) -> String {
    let (id, field, value) = match (args.first(), args.get(1)) {
        (Some(id), Some(field)) => (*id, *field, args[2..].join(" ")),
        _ => return "Usage: /cron edit <id> <expr|name|cmd|prompt|model> <value>".to_string(),
    };
    let mut patch = CronJobPatch::default();
    match field {
        "expr" => {
            // Preserve the existing timezone when only the expression changes.
            let tz = match cron::get_job(config, id) {
                Ok(j) => match j.schedule { Schedule::Cron { tz, .. } => tz, _ => None },
                Err(e) => return format!("✗ {e}"),
            };
            patch.schedule = Some(Schedule::Cron { expr: value, tz });
        }
        "name" => patch.name = Some(value),
        "cmd" => patch.command = Some(value),
        "prompt" => patch.prompt = Some(value),
        "model" => patch.model = Some(value),
        other => return format!("Unknown field '{other}'. Use expr|name|cmd|prompt|model."),
    }
    match cron::update_job(config, id, patch) {
        Ok(job) => format!("✅ Updated cron job {}\n  Expr: {}\n  Next: {}", job.id, job.expression, job.next_run.to_rfc3339()),
        Err(e) => format!("✗ {e}"),
    }
}

/// Pull `--flag <value>` out of the token list, returning (remaining, value?).
fn extract_flag<'a>(args: &[&'a str], flag: &str) -> (Vec<&'a str>, Option<String>) {
    if let Some(i) = args.iter().position(|a| *a == flag) {
        if let Some(v) = args.get(i + 1) {
            let mut rest: Vec<&str> = args.to_vec();
            rest.drain(i..=i + 1);
            return (rest, Some((*v).to_string()));
        }
    }
    (args.to_vec(), None)
}
```

- [ ] **Step 4 — Run, confirm PASS.** `cargo test --lib tui::commands::cron`
- [ ] **Step 5 — Commit.** `git commit -m "feat(tui): wire /cron to the store — shell+agent create, edit, remove/pause/resume"`

---

## Task 2 — Interactive picker + detail panel with action-keys (run / pause / delete)

**Files:** `list_picker.rs` (`Cron` variant), `info_panel.rs` (`cron_job_id` field),
`app.rs` (dispatch arm + `build_cron_detail_panel` + 3 action-key arms + 3 helpers),
`cron.rs` (`build_cron_picker`).

- [ ] **Step 1 — `ListPickerKind::Cron`** — add after `Autonomy` (`list_picker.rs:44`):

```rust
    /// Scheduled-jobs browser opened via `/cron` (no arg). Enter opens a detail
    /// panel whose action-keys run/pause/delete the selected job.
    Cron,
```

- [ ] **Step 2 — `build_cron_picker`** in `cron.rs` (+ test mirroring
  `autonomy.rs::no_arg_opens_picker`):

```rust
use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

fn build_cron_picker(config: &Config) -> ListPicker {
    let items: Vec<ListPickerItem> = cron::list_jobs(config).unwrap_or_default().into_iter().map(|j| {
        let name = j.name.clone().unwrap_or_else(|| j.id[..j.id.len().min(8)].to_string());
        ListPickerItem {
            key: j.id.clone(),
            primary: format!("{name} [{}]", if j.enabled { "on" } else { "paused" }),
            secondary: format!("{} · next {} · {}", j.expression, j.next_run.to_rfc3339(), j.last_status.as_deref().unwrap_or("never run")),
        }
    }).collect();
    ListPicker::new(ListPickerKind::Cron, "Scheduled Jobs", items, None,
        "No cron jobs yet — /cron add <5-field-expr> <cmd> to create one.")
}
```
```rust
    #[test]
    fn build_cron_picker_lists_jobs() {
        let tmp = TempDir::new().unwrap();
        let c = cfg(&tmp);
        run_cron_text(&c, "add */5 * * * * echo hi");
        let id = first_id(&c);
        let p = build_cron_picker(&c);
        assert_eq!(p.kind, crate::tui::widgets::ListPickerKind::Cron);
        let keys: Vec<String> = p.entries().iter().filter_map(|e| e.as_item().map(|i| i.key.clone())).collect();
        assert_eq!(keys, vec![id]);
    }
```

- [ ] **Step 3 — `InfoPanel` action context.** In `info_panel.rs`, add to the struct
  (`:210-219`) and `new` (`:222-229`):

```rust
    /// When set, this panel is a cron-job detail: the app's info-panel key
    /// handler enables run/pause/delete action-keys for this job id.
    pub cron_job_id: Option<String>,
```
```rust
        // in `new`, initialise:
            cron_job_id: None,
```
Add a builder:
```rust
    pub fn with_cron_actions<S: Into<String>>(mut self, job_id: S) -> Self {
        self.cron_job_id = Some(job_id.into());
        self
    }
```

- [ ] **Step 4 — `build_cron_detail_panel`** — a method on the app that both the
  dispatch arm and the action helpers use to (re)build the detail view. Add near
  `dispatch_list_picker_selection` in `app.rs`:

```rust
    fn build_cron_detail_panel(config: &crate::config::Config, job_id: &str) -> Option<super::widgets::InfoPanel> {
        use super::widgets::{InfoPanel, InfoSection, StatusKind};
        let job = crate::cron::get_job(config, job_id).ok()?;
        let mut detail = InfoSection::new("Job")
            .status_with(if job.enabled { StatusKind::Ok } else { StatusKind::Warn }, "State",
                if job.enabled { "enabled" } else { "paused" })
            .status_with(StatusKind::Info, "Schedule", job.expression.clone())
            .status_with(StatusKind::Info, "Next run", job.next_run.to_rfc3339())
            .status_with(StatusKind::Info, "Last run",
                job.last_run.map_or("never".to_string(), |d| d.to_rfc3339()))
            .status_with(match job.last_status.as_deref() {
                Some("ok") => StatusKind::Ok, Some("error") => StatusKind::Fail, _ => StatusKind::Info,
            }, "Last status", job.last_status.clone().unwrap_or_else(|| "n/a".into()));
        let what = if job.command.is_empty() { job.prompt.clone().unwrap_or_default() } else { job.command.clone() };
        detail = detail.status_with(StatusKind::Info,
            if job.command.is_empty() { "Prompt" } else { "Command" }, what);

        let mut history = InfoSection::new("Recent runs");
        match crate::cron::list_runs(config, job_id, 5) {
            Ok(runs) if runs.is_empty() => history = history.status_with(StatusKind::Info, "—", "no runs yet"),
            Ok(runs) => for r in runs {
                history = history.status_with(if r.status == "ok" { StatusKind::Ok } else { StatusKind::Fail },
                    r.started_at.to_rfc3339(), format!("{} ({}ms)", r.status, r.duration_ms.unwrap_or(0)));
            },
            Err(e) => history = history.status_with(StatusKind::Fail, "history", e.to_string()),
        }
        Some(InfoPanel::new("Cron Job")
            .with_subtitle(&job_id[..job_id.len().min(8)])
            .with_footer("[r] run · [p] pause/resume · [d] delete · Esc close")
            .with_cron_actions(job_id)
            .section(detail)
            .section(history))
    }
```

  Add the dispatch arm (in `dispatch_list_picker_selection`, after `Autonomy`):

```rust
            ListPickerKind::Cron => {
                match Self::load_config_for_cron() {
                    Ok(config) => match Self::build_cron_detail_panel(&config, &key) {
                        Some(panel) => self.info_panel = Some(panel),
                        None => self.cron_system_msg(&format!("Cron job {key} not found.")),
                    },
                    Err(e) => self.cron_system_msg(&format!("Could not load config: {e}")),
                }
            }
```
  where `load_config_for_cron` + `cron_system_msg` are small private helpers:
```rust
    fn load_config_for_cron() -> anyhow::Result<crate::config::Config> {
        let h = tokio::runtime::Handle::try_current()?;
        tokio::task::block_in_place(|| h.block_on(async { crate::config::Config::load_or_init().await }))
    }
    fn cron_system_msg(&mut self, msg: &str) {
        let _ = self.context.append_system_message(msg);
        self.scrollback_queue.push(("system".into(), msg.to_string()));
    }
```

- [ ] **Step 5 — Action-key arms.** In `app.rs` key handling, insert BEFORE the
  catch-all `_ if self.info_panel.is_some()` (line 995):

```rust
            KeyCode::Char('r') | KeyCode::Char('R')
                if self.info_panel.as_ref().is_some_and(|p| p.cron_job_id.is_some()) => {
                self.cron_panel_action('r');
                return Ok(EventResult::Continue);
            }
            KeyCode::Char('p') | KeyCode::Char('P')
                if self.info_panel.as_ref().is_some_and(|p| p.cron_job_id.is_some()) => {
                self.cron_panel_action('p');
                return Ok(EventResult::Continue);
            }
            KeyCode::Char('d') | KeyCode::Char('D')
                if self.info_panel.as_ref().is_some_and(|p| p.cron_job_id.is_some()) => {
                self.cron_panel_action('d');
                return Ok(EventResult::Continue);
            }
```
  And the handler (async-free; run-now uses `run_job_manual` via block_on):
```rust
    fn cron_panel_action(&mut self, action: char) {
        let Some(id) = self.info_panel.as_ref().and_then(|p| p.cron_job_id.clone()) else { return };
        let config = match Self::load_config_for_cron() {
            Ok(c) => c,
            Err(e) => { self.cron_system_msg(&format!("Could not load config: {e}")); return; }
        };
        match action {
            'r' => {
                // Run DETACHED — a shell job (up to 120s) or agent job (up to 600s)
                // must NOT block the render loop. `block_on`-ing a full agent turn
                // here would freeze the whole TUI for its duration. Spawn instead;
                // `run_job_manual` records to run history, and the user reopens the
                // job to see the result.
                let job = match crate::cron::get_job(&config, &id) { Ok(j) => j, Err(e) => { self.cron_system_msg(&format!("✗ {e}")); return; } };
                let cfg = config.clone();
                tokio::spawn(async move { let _ = crate::cron::scheduler::run_job_manual(&cfg, &job).await; });
                self.cron_system_msg(&format!("▶ Started run of cron job {id} — reopen the job for the result."));
            }
            'p' => {
                let enabled = crate::cron::get_job(&config, &id).map(|j| j.enabled).unwrap_or(false);
                let r = if enabled { crate::cron::pause_job(&config, &id) } else { crate::cron::resume_job(&config, &id) };
                match r { Ok(_) => self.info_panel = Self::build_cron_detail_panel(&config, &id),
                          Err(e) => self.cron_system_msg(&format!("✗ {e}")) }
            }
            'd' => {
                match crate::cron::remove_job(&config, &id) {
                    Ok(()) => { self.info_panel = None; self.cron_system_msg(&format!("🗑 Removed cron job {id}")); }
                    Err(e) => self.cron_system_msg(&format!("✗ {e}")),
                }
            }
            _ => {}
        }
    }
```
  > `load_config_for_cron`/`cron_system_msg`/`build_cron_detail_panel` are the
  > associated/`&mut self` helpers added in Step 4. `build_cron_detail_panel` is
  > an **associated fn** (no `self`) so it can be called while `self.info_panel`
  > is being reassigned without a borrow conflict.

- [ ] **Step 6 — Verify.** `cargo test --lib tui::commands::cron tui::widgets::info_panel`
  + `cargo build --lib` (the `Cron` arm makes `dispatch_list_picker_selection`'s
  `match kind` exhaustive again; `list_picker.rs` rendering uses `if self.kind ==`
  guards, NOT an exhaustive match, so it needs no `Cron` arm). Drive the TUI:
  `/cron` → pick → detail; `r` starts a detached run (reopen to see the history
  row), `p` toggles pause, `d` removes + closes.
- [ ] **Step 7 — Commit.** `git commit -m "feat(tui): interactive /cron picker + detail panel with run/pause/delete action-keys"`

---

## Task 3 — `/doctor` scheduler-health line (from daemon_state.json)

**Files:** `src/tui/commands/config.rs`.

- [ ] **Step 1 — Failing test** for a pure `scheduler_diag(&Path) -> (StatusKind, String)`:

```rust
    #[test]
    fn scheduler_diag_reads_state_file() {
        use super::scheduler_diag;
        use crate::tui::widgets::StatusKind;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("daemon_state.json");
        assert!(matches!(scheduler_diag(&path).0, StatusKind::Warn)); // absent → daemon down
        std::fs::write(&path, serde_json::json!({"components":{"scheduler":{"status":"ok"}}}).to_string()).unwrap();
        assert!(matches!(scheduler_diag(&path).0, StatusKind::Ok));
        std::fs::write(&path, serde_json::json!({"components":{"scheduler":{"status":"error"}}}).to_string()).unwrap();
        assert!(matches!(scheduler_diag(&path).0, StatusKind::Fail));
    }
```

- [ ] **Step 2 — Run, confirm FAIL.**

- [ ] **Step 3 — Implement** in `config.rs`:

```rust
fn scheduler_diag(state_file: &std::path::Path) -> (StatusKind, String) {
    let Ok(raw) = std::fs::read_to_string(state_file) else {
        return (StatusKind::Warn, "daemon not running (start `rantaiclaw daemon` for scheduled jobs)".to_string());
    };
    let status = serde_json::from_str::<serde_json::Value>(&raw).ok()
        .and_then(|v| v.get("components")?.get("scheduler")?.get("status")?.as_str().map(str::to_string));
    match status.as_deref() {
        Some("ok") => (StatusKind::Ok, "healthy".to_string()),
        Some("error") => (StatusKind::Fail, "unhealthy — see daemon logs".to_string()),
        Some(other) => (StatusKind::Warn, other.to_string()),
        None => (StatusKind::Warn, "not tracked yet".to_string()),
    }
}
```
  In `DoctorCommand::execute`, before the roll-up (`config.rs:273`), build a
  Scheduler section (config loaded via block_in_place → `state_file_path`):
```rust
        let scheduler_section = {
            let path = tokio::runtime::Handle::try_current().ok().map(|h| {
                tokio::task::block_in_place(|| h.block_on(async {
                    crate::config::Config::load_or_init().await.map(|c| crate::daemon::state_file_path(&c))
                }))
            });
            let mut s = InfoSection::new("Scheduler");
            match path {
                Some(Ok(p)) => { let (k, m) = scheduler_diag(&p); s = s.status_with(k, "Cron scheduler", m); }
                _ => s = s.status_with(StatusKind::Warn, "Cron scheduler", "could not resolve state file"),
            }
            s
        };
```
  Add `&scheduler_section` to the `sections` verdict array (`config.rs:281`) and
  `.section(scheduler_section)` to the `InfoPanel` (`config.rs:292-298`). A
  down/absent daemon → **Warn**, not Fail (matches the keyless-provider convention).

- [ ] **Step 4 — Run + build.** `cargo test --lib tui::commands::config` + `cargo build --lib`. Drive `/doctor`.
- [ ] **Step 5 — Commit.** `git commit -m "feat(tui): show scheduler health in /doctor (from daemon_state.json)"`

---

## Done criteria
- [ ] `/cron` reaches parity with the web console: create shell + agent, edit
  (expr/name/cmd/prompt/model), pause/resume, run-now, delete, browse + history.
- [ ] Hybrid UX: `/cron` opens a picker; the detail panel's `[r]/[p]/[d]` keys act;
  `/cron add [--agent] …` + `/cron edit <id> <field> <value>` cover multi-field ops.
- [ ] `/doctor` shows a scheduler row from `daemon_state.json`.
- [ ] `cargo test --lib tui::commands::cron tui::commands::config tui::widgets::info_panel` green; `cargo build --lib`; fmt + scoped clippy clean.
- [ ] No schema/exposure change.

## STOP conditions
- `list_picker.rs` rendering does NOT have an exhaustive `match self.kind` (it uses
  `if self.kind == …ClawhubInstall/Skill` guards), so the new `Cron` variant needs
  **no** render arm — it falls through to normal-item rendering. The ONLY exhaustive
  `match` on `ListPickerKind` is `dispatch_list_picker_selection` (app.rs:2706); the
  plan's `Cron` arm makes it exhaustive again.
- `run_job_manual` (026 Task 5) is absent today, so run-now (`r`) won't compile —
  land 026 first. Do NOT substitute the existing `execute_job_now` (scheduler.rs:51,
  identical signature): it does NOT record a run, so the detail panel's history
  wouldn't reflect the run. `run_job_manual` must record (026 Task 5 defines it that
  way). If 026 truly slips, use `execute_job_now` AND add an explicit `record_run` +
  `record_last_run` call after it (i.e. reproduce `run_job_manual` inline).
- The `r` action is spawned DETACHED (not `block_on`), so a long agent/shell run
  never freezes the render loop; the config-load `block_in_place` (brief file read)
  is the only blocking call and is acceptable. Do NOT reintroduce `block_on` on the
  run itself.
- Unit-test the pure helpers (`run_cron_text`, `add_text`, `edit_text`,
  `build_cron_picker`, `scheduler_diag`); the `app.rs` key wiring is verified by
  driving the TUI (no unit seam).

## Rollback
Per-commit revert. Enum variant, `InfoPanel` field, dispatch arm, key arms, doctor
section are additive; reverting Task 1 restores the stub. No persisted-state/schema change.
