use std::fmt::Write as _;

use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::config::Config;
use crate::cron::{self, CronJobPatch, Schedule, SessionTarget};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

/// /cron command — manage scheduled tasks
pub struct CronCommand;

impl CommandHandler for CronCommand {
    fn name(&self) -> &str {
        "cron"
    }

    fn description(&self) -> &str {
        "Manage scheduled tasks"
    }

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
    let sub = parts.first().copied().unwrap_or("list");
    // Every write door refuses while the cron feature switch is off — with
    // the same words the CLI, the HTTP API and the agent tools use. Reads
    // (`list`) stay open, so an operator can still inspect dormant jobs. The
    // refusal comes before the arguments are looked at: the door is closed.
    if matches!(sub, "add" | "edit" | "remove" | "pause" | "resume") && !config.cron.enabled {
        return format!("✗ {}", crate::cron::CRON_DISABLED_MESSAGE);
    }
    match sub {
        "list" | "" => list_text(config),
        "add" => add_text(config, &parts[1..]),
        "edit" => edit_text(config, &parts[1..]),
        "remove" => id_op(
            &parts,
            |id| cron::remove_job(config, id).map(|()| format!("🗑 Removed cron job {id}")),
            "remove",
        ),
        "pause" => id_op(
            &parts,
            |id| cron::pause_job(config, id).map(|_| format!("⏸ Paused cron job {id}")),
            "pause",
        ),
        "resume" => id_op(
            &parts,
            |id| cron::resume_job(config, id).map(|_| format!("▶ Resumed cron job {id}")),
            "resume",
        ),
        other => format!(
            "Unknown cron subcommand: {other}\n\nUsage: /cron [list|add|edit|remove|pause|resume]"
        ),
    }
}

fn id_op(parts: &[&str], f: impl FnOnce(&str) -> anyhow::Result<String>, name: &str) -> String {
    match parts.get(1) {
        None => format!("Usage: /cron {name} <id>"),
        Some(id) => f(id).unwrap_or_else(|e| format!("✗ {e}")),
    }
}

fn list_text(config: &Config) -> String {
    let banner = if !config.cron.enabled {
        "⚠️  Scheduler disabled (cron.enabled=false) — jobs will NOT fire.\n"
    } else if !config.scheduler.enabled {
        "⚠️  Scheduler loop disabled (scheduler.enabled=false) — jobs will NOT fire.\n"
    } else {
        ""
    };
    match cron::list_jobs(config) {
        Ok(jobs) if jobs.is_empty() => {
            format!("{banner}Scheduled tasks:\n  No cron jobs configured.\n\nUse /cron add <5-field-expr> <cmd> to create one.")
        }
        Ok(jobs) => {
            let mut out = format!("{banner}Scheduled tasks ({}):\n", jobs.len());
            for j in jobs {
                // Every row shows the job id, so the operator can copy it and
                // pass it back to `/cron pause|resume|remove|edit`. The name
                // is kept when the job has one.
                let display_id = cron::cron_display_id(&j);
                let name = j.name.clone().unwrap_or_default();
                let name_part = if name.is_empty() {
                    String::new()
                } else {
                    format!("{} ", crate::memory::sanitize::sanitize_for_terminal(&name))
                };
                let what = if j.command.is_empty() {
                    j.prompt.clone().unwrap_or_default()
                } else {
                    j.command.clone()
                };
                let _ = write!(
                    out,
                    "  {name_part}[{display_id}] [{}] {} · next {} · {}\n    {}\n",
                    if j.enabled { "on" } else { "paused" },
                    // Render the structured schedule (Display) — the stored
                    // `expression` is empty for at/every jobs — and keep the
                    // terminal-escape sanitize from plan 178.
                    crate::memory::sanitize::sanitize_for_terminal(&j.schedule.to_string()),
                    j.next_run.to_rfc3339(),
                    j.last_status.as_deref().unwrap_or("never run"),
                    crate::memory::sanitize::sanitize_for_terminal(&what),
                );
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
        cron::add_agent_job(
            config,
            None,
            schedule,
            &payload,
            SessionTarget::Isolated,
            model,
            None,
            false,
            Some("tui"),
            None,
            None,
            None,
        )
    } else {
        cron::add_shell_job(
            config,
            None,
            schedule,
            &payload,
            None,
            false,
            Some("tui"),
            None,
            None,
            None,
        )
    };
    match result {
        Ok(job) => format!(
            "✅ Added cron job {}\n  Expr: {}\n  Next: {}",
            job.id,
            job.schedule,
            job.next_run.to_rfc3339()
        ),
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
                Ok(j) => match j.schedule {
                    Schedule::Cron { tz, .. } => tz,
                    _ => None,
                },
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
        Ok(job) => format!(
            "✅ Updated cron job {}\n  Expr: {}\n  Next: {}",
            job.id,
            job.schedule,
            job.next_run.to_rfc3339()
        ),
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

/// Build the interactive jobs picker (opened by `/cron` with no arg or `/cron list`).
pub fn build_cron_picker(config: &Config) -> ListPicker {
    let items: Vec<ListPickerItem> = cron::list_jobs(config)
        .unwrap_or_default()
        .into_iter()
        .map(|j| {
            // The row shows the same short id every other surface prints, so
            // the operator can copy it and paste it into a `/cron` command.
            let display_id = cron::cron_display_id(&j);
            let name = j.name.clone().unwrap_or_default();
            let name_part = if name.is_empty() {
                String::new()
            } else {
                format!("{} ", crate::memory::sanitize::sanitize_for_terminal(&name))
            };
            ListPickerItem {
                key: j.id.clone(),
                primary: format!(
                    "{name_part}[{display_id}] [{}]",
                    if j.enabled { "on" } else { "paused" }
                ),
                secondary: format!(
                    "{} · next {} · {}",
                    j.schedule,
                    j.next_run.to_rfc3339(),
                    j.last_status.as_deref().unwrap_or("never run")
                ),
                disabled: false,
            }
        })
        .collect();
    ListPicker::new(
        ListPickerKind::Cron,
        "Scheduled Jobs",
        items,
        None,
        "No cron jobs yet — /cron add <5-field-expr> <cmd> to create one.",
    )
}

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
        assert_eq!(
            crate::cron::get_job(&c, &id).unwrap().expression,
            "0 8 * * *"
        );
        run_cron_text(&c, &format!("edit {id} name morning"));
        assert_eq!(
            crate::cron::get_job(&c, &id).unwrap().name.as_deref(),
            Some("morning")
        );
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
    #[test]
    fn build_cron_picker_lists_jobs() {
        let tmp = TempDir::new().unwrap();
        let c = cfg(&tmp);
        run_cron_text(&c, "add */5 * * * * echo hi");
        let id = first_id(&c);
        let p = build_cron_picker(&c);
        assert_eq!(p.kind, crate::tui::widgets::ListPickerKind::Cron);
        let keys: Vec<String> = p
            .entries()
            .iter()
            .filter_map(|e| e.as_item().map(|i| i.key.clone()))
            .collect();
        assert_eq!(keys, vec![id]);
    }

    /// Every write door of `/cron` refuses while the cron feature switch is
    /// off, with the same words every other door uses — and nothing changes.
    ///
    /// The first group passes arguments that would otherwise produce a
    /// *different* answer (a usage line, a parse error). The door is closed
    /// before it looks at the arguments at all, so the refusal is the whole
    /// answer. That is what makes this test tight: the store refuses with the
    /// same words, so a message that merely "contains the refusal" would pass
    /// even with this gate removed.
    #[test]
    fn every_write_door_refuses_when_cron_is_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut c = cfg(&tmp);
        run_cron_text(&c, "add */5 * * * * echo hi");
        let id = first_id(&c);
        c.cron.enabled = false;

        let refusal = format!("✗ {}", crate::cron::CRON_DISABLED_MESSAGE);

        // Arguments that would otherwise produce a different answer.
        for args in [
            "add",
            "edit",
            "remove",
            "pause",
            "resume",
            "add not-a-schedule",
            "remove 0000000000000000",
        ] {
            let out = run_cron_text(&c, args);
            assert_eq!(
                out, refusal,
                "`/cron {args}` must refuse with the refusal and nothing else"
            );
        }

        // The same doors, with a real id: the refusal, and the job is
        // untouched — it was neither edited, paused nor removed.
        for args in [
            "add */5 * * * * echo hi".to_string(),
            format!("edit {id} name renamed"),
            format!("remove {id}"),
            format!("pause {id}"),
            format!("resume {id}"),
        ] {
            let out = run_cron_text(&c, &args);
            assert!(
                out.contains(crate::cron::CRON_DISABLED_MESSAGE),
                "`/cron {args}` must refuse: {out}"
            );
        }

        let job = crate::cron::get_job(&c, &id).unwrap();
        assert!(job.enabled);
        assert_eq!(job.name, None);
        assert_eq!(crate::cron::list_jobs(&c).unwrap().len(), 1);
    }

    /// `/cron list` shows the id every surface prints, so the operator can
    /// copy it and pass it back to a `/cron` command.
    #[test]
    fn list_shows_the_reusable_job_id() {
        let tmp = TempDir::new().unwrap();
        let c = cfg(&tmp);
        run_cron_text(&c, "add */5 * * * * echo hi");
        let job = &crate::cron::list_jobs(&c).unwrap()[0];
        let printed = crate::cron::cron_display_id(job);

        let listed = run_cron_text(&c, "list");
        assert!(listed.contains(&printed), "{listed}");
    }

    /// The picker's row shows the same id, so the operator can copy it there
    /// too.
    #[test]
    fn the_picker_row_shows_the_reusable_job_id() {
        let tmp = TempDir::new().unwrap();
        let c = cfg(&tmp);
        run_cron_text(&c, "add */5 * * * * echo hi");
        let job = &crate::cron::list_jobs(&c).unwrap()[0];
        let printed = crate::cron::cron_display_id(job);

        let picker = build_cron_picker(&c);
        let primary = picker.entries()[0]
            .as_item()
            .map(|i| i.primary.clone())
            .unwrap_or_default();
        assert!(primary.contains(&printed), "{primary}");
    }
}
