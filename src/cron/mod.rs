use crate::config::Config;
use crate::security::SecurityPolicy;
use anyhow::{bail, Result};

mod schedule;
mod store;
mod types;

pub mod scheduler;

#[allow(unused_imports)]
pub use schedule::{
    next_run_for_schedule, normalize_expression, schedule_cron_expression, validate_schedule,
};
#[allow(unused_imports)]
pub use store::{
    add_agent_job, add_job, add_shell_job, due_jobs, get_job, list_jobs, list_runs,
    record_last_run, record_run, record_run_attempt, remove_job, reschedule_after_run, update_job,
};
pub use types::{CronJob, CronJobPatch, CronRun, DeliveryConfig, JobType, Schedule, SessionTarget};

#[allow(clippy::needless_pass_by_value)]
pub fn handle_command(command: crate::CronCommands, config: &Config) -> Result<()> {
    // Mutating cron actions require the feature switch, mirroring the `cron_*`
    // tools. Reads (List, Runs) stay open so an operator can inspect dormant jobs.
    let mutating = matches!(
        command,
        crate::CronCommands::Add { .. }
            | crate::CronCommands::AddAt { .. }
            | crate::CronCommands::AddEvery { .. }
            | crate::CronCommands::Once { .. }
            | crate::CronCommands::Update { .. }
            | crate::CronCommands::Remove { .. }
            | crate::CronCommands::Pause { .. }
            | crate::CronCommands::Resume { .. }
            | crate::CronCommands::Run { .. }
    );
    if mutating && !config.cron.enabled {
        bail!("cron is disabled by config (cron.enabled=false)");
    }
    match command {
        crate::CronCommands::List => {
            if !config.cron.enabled {
                println!(
                    "⚠️  Scheduler disabled (cron.enabled=false) — listed jobs will NOT fire until you re-enable it."
                );
            } else if !config.scheduler.enabled {
                println!(
                    "⚠️  Scheduler loop disabled (scheduler.enabled=false) — listed jobs will NOT fire until you re-enable it."
                );
            }
            let jobs = list_jobs(config)?;
            if jobs.is_empty() {
                println!("No scheduled tasks yet.");
                println!("\nUsage:");
                println!("  rantaiclaw cron add '0 9 * * *' 'agent -m \"Good morning!\"'");
                return Ok(());
            }

            crate::cli_style::section(&format!("scheduled jobs ({})", jobs.len()));
            for job in jobs {
                let last_run = job
                    .last_run
                    .map_or_else(|| "never".into(), |d| d.to_rfc3339());
                let last_status = job.last_status.unwrap_or_else(|| "n/a".into());
                let short = &job.id[..job.id.len().min(8)];
                println!(
                    "  {} {short}  {}",
                    crate::cli_style::dot(job.enabled),
                    crate::memory::sanitize::sanitize_for_terminal(&job.schedule.to_string())
                );
                let payload = if job.command.is_empty() {
                    job.prompt.clone().unwrap_or_default()
                } else {
                    job.command.clone()
                };
                if !payload.is_empty() {
                    println!(
                        "       {}",
                        crate::cli_style::dim(&crate::memory::sanitize::sanitize_for_terminal(
                            &payload
                        ))
                    );
                }
                println!(
                    "       {}",
                    crate::cli_style::dim(&format!(
                        "next {}  ·  last {last_run} ({last_status})",
                        job.next_run.to_rfc3339()
                    ))
                );
            }
            Ok(())
        }
        crate::CronCommands::Add {
            expression,
            tz,
            command,
            agent,
            model,
        } => {
            let schedule = Schedule::Cron {
                expr: expression,
                tz,
            };
            let job = add_scheduled(config, schedule, &command, agent, model, false)?;
            print_added(&job);
            Ok(())
        }
        crate::CronCommands::AddAt {
            at,
            command,
            agent,
            model,
        } => {
            let at = chrono::DateTime::parse_from_rfc3339(&at)
                .map_err(|e| anyhow::anyhow!("Invalid RFC3339 timestamp for --at: {e}"))?
                .with_timezone(&chrono::Utc);
            let schedule = Schedule::At { at };
            // Agent one-shots auto-delete on success (matching the cron_add tool);
            // shell one-shots stay (026 disables them after firing).
            let job = add_scheduled(config, schedule, &command, agent, model, agent)?;
            print_added(&job);
            Ok(())
        }
        crate::CronCommands::AddEvery {
            every_ms,
            command,
            agent,
            model,
        } => {
            let schedule = Schedule::Every { every_ms };
            let job = add_scheduled(config, schedule, &command, agent, model, false)?;
            print_added(&job);
            Ok(())
        }
        crate::CronCommands::Once {
            delay,
            command,
            agent,
            model,
        } => {
            let at = chrono::Utc::now()
                .checked_add_signed(parse_delay(&delay)?)
                .ok_or_else(|| anyhow::anyhow!("delay too large: {delay}"))?;
            let job = add_scheduled(config, Schedule::At { at }, &command, agent, model, agent)?;
            print_added(&job);
            Ok(())
        }
        crate::CronCommands::Update {
            id,
            expression,
            tz,
            command,
            name,
        } => {
            if expression.is_none() && tz.is_none() && command.is_none() && name.is_none() {
                bail!("At least one of --expression, --tz, --command, or --name must be provided");
            }

            // Merge expression/tz with the existing schedule so that
            // --tz alone updates the timezone and --expression alone
            // preserves the existing timezone.
            let schedule = if expression.is_some() || tz.is_some() {
                let existing = get_job(config, &id)?;
                let (existing_expr, existing_tz) = match existing.schedule {
                    Schedule::Cron {
                        expr,
                        tz: existing_tz,
                    } => (expr, existing_tz),
                    _ => bail!("Cannot update expression/tz on a non-cron schedule"),
                };
                Some(Schedule::Cron {
                    expr: expression.unwrap_or(existing_expr),
                    tz: tz.or(existing_tz),
                })
            } else {
                None
            };

            if let Some(ref cmd) = command {
                let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
                if !security.is_command_allowed(cmd) {
                    bail!("Command blocked by security policy: {cmd}");
                }
            }

            let patch = CronJobPatch {
                schedule,
                command,
                name,
                ..CronJobPatch::default()
            };

            let job = update_job(config, &id, patch)?;
            println!("\u{2705} Updated cron job {}", job.id);
            println!("  Expr: {}", job.expression);
            println!("  Next: {}", job.next_run.to_rfc3339());
            println!(
                "  Cmd : {}",
                crate::memory::sanitize::sanitize_for_terminal(&job.command)
            );
            Ok(())
        }
        crate::CronCommands::Remove { id } => {
            remove_job(config, &id)?;
            println!("\u{2705} Removed cron job {id}");
            Ok(())
        }
        crate::CronCommands::Pause { id } => {
            pause_job(config, &id)?;
            println!("⏸️  Paused cron job {id}");
            Ok(())
        }
        crate::CronCommands::Resume { id } => {
            resume_job(config, &id)?;
            println!("▶️  Resumed cron job {id}");
            Ok(())
        }
        crate::CronCommands::Run { id } => {
            let handle = tokio::runtime::Handle::try_current()
                .map_err(|_| anyhow::anyhow!("cron run must execute inside the tokio runtime"))?;
            let report =
                tokio::task::block_in_place(|| handle.block_on(run_job_report(config, &id)))?;
            println!("{report}");
            Ok(())
        }
        crate::CronCommands::Runs { id, limit } => {
            println!("{}", runs_report(config, &id, limit)?);
            Ok(())
        }
    }
}

/// Create a shell or agent job from a resolved schedule. `delete_after_run`
/// controls whether a one-shot (`At`) self-deletes on success; it is honored
/// for both shell and agent jobs.
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
            config,
            None,
            schedule,
            payload,
            SessionTarget::Isolated,
            model,
            None,
            delete_after_run,
            Some("cli"),
        )
    } else {
        add_shell_job(
            config,
            None,
            schedule,
            payload,
            None,
            delete_after_run,
            Some("cli"),
        )
    }
}

fn print_added(job: &CronJob) {
    println!("✅ Added cron job {}", job.id);
    // Schedule-aware line — `job.expression` is "" for At/Every, so don't print
    // an empty `Expr:`.
    match &job.schedule {
        Schedule::Cron { expr, .. } => println!("  Expr: {expr}"),
        Schedule::At { at } => println!("  At  : {}", at.to_rfc3339()),
        Schedule::Every { every_ms } => println!("  Every(ms): {every_ms}"),
    }
    println!("  Next: {}", job.next_run.to_rfc3339());
}

/// Force-run a job and return a human report. Reuses the shared manual-run path
/// (records run history; does not reschedule/deliver) — same semantics as the
/// `cron_run` tool and the web `POST /cron/{id}/run`. NOTE: keep this NON-`pub`
/// (only `handle_command` + the in-module test call it); a `pub` fn returning
/// `Result` would trip `clippy::missing_errors_doc` under the pedantic gate.
async fn run_job_report(config: &Config, id: &str) -> Result<String> {
    let job = get_job(config, id)?;
    let security =
        crate::security::SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
    let (ok, output) = crate::cron::scheduler::run_job_manual(config, &security, &job).await;
    Ok(format!(
        "{} cron job {id} ({})\n{}",
        if ok { "✅" } else { "✗" },
        if ok { "ok" } else { "error" },
        output.trim(),
    ))
}

fn runs_report(config: &Config, id: &str, limit: usize) -> Result<String> {
    use std::fmt::Write as _;
    let runs = list_runs(config, id, limit)?;
    if runs.is_empty() {
        return Ok(format!("No run history for cron job {id}."));
    }
    let mut out = format!("Run history for {id} ({}):\n", runs.len());
    for r in runs {
        let _ = writeln!(
            out,
            "  {} · {} · {}ms",
            r.started_at.to_rfc3339(),
            r.status,
            r.duration_ms.unwrap_or(0),
        );
    }
    Ok(out)
}

pub fn add_once(config: &Config, delay: &str, command: &str) -> Result<CronJob> {
    let duration = parse_delay(delay)?;
    // `now + duration` can still overflow the DateTime range even when the
    // Duration itself is representable, so add with a checked op.
    let at = chrono::Utc::now()
        .checked_add_signed(duration)
        .ok_or_else(|| anyhow::anyhow!("delay too large: {delay}"))?;
    add_once_at(config, at, command)
}

pub fn add_once_at(
    config: &Config,
    at: chrono::DateTime<chrono::Utc>,
    command: &str,
) -> Result<CronJob> {
    let schedule = Schedule::At { at };
    add_shell_job(config, None, schedule, command, None, false, Some("cli"))
}

pub fn pause_job(config: &Config, id: &str) -> Result<CronJob> {
    update_job(
        config,
        id,
        CronJobPatch {
            enabled: Some(false),
            ..CronJobPatch::default()
        },
    )
}

pub fn resume_job(config: &Config, id: &str) -> Result<CronJob> {
    update_job(
        config,
        id,
        CronJobPatch {
            enabled: Some(true),
            ..CronJobPatch::default()
        },
    )
}

fn parse_delay(input: &str) -> Result<chrono::Duration> {
    let input = input.trim();
    if input.is_empty() {
        anyhow::bail!("delay must not be empty");
    }
    let split = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    let (num, unit) = input.split_at(split);
    let amount: i64 = num.parse()?;
    let unit = if unit.is_empty() { "m" } else { unit };
    // Non-panicking constructors: chrono's `Duration::minutes/hours/days` panic
    // on values that overflow its internal millisecond range. `num.parse::<i64>()`
    // above already errors on a string too large for i64; the `try_*` mapping
    // handles the in-i64-but-out-of-Duration-range case.
    let duration = match unit {
        "s" => chrono::Duration::try_seconds(amount),
        "m" => chrono::Duration::try_minutes(amount),
        "h" => chrono::Duration::try_hours(amount),
        "d" => chrono::Duration::try_days(amount),
        _ => anyhow::bail!("unsupported delay unit '{unit}', use s/m/h/d"),
    }
    .ok_or_else(|| anyhow::anyhow!("delay too large: {input}"))?;
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    fn make_job(config: &Config, expr: &str, tz: Option<&str>, cmd: &str) -> CronJob {
        add_shell_job(
            config,
            None,
            Schedule::Cron {
                expr: expr.into(),
                tz: tz.map(Into::into),
            },
            cmd,
            None,
            false,
            None,
        )
        .unwrap()
    }

    fn run_update(
        config: &Config,
        id: &str,
        expression: Option<&str>,
        tz: Option<&str>,
        command: Option<&str>,
        name: Option<&str>,
    ) -> Result<()> {
        handle_command(
            crate::CronCommands::Update {
                id: id.into(),
                expression: expression.map(Into::into),
                tz: tz.map(Into::into),
                command: command.map(Into::into),
                name: name.map(Into::into),
            },
            config,
        )
    }

    #[test]
    fn update_changes_command_via_handler() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", None, "echo original");

        run_update(&config, &job.id, None, None, Some("echo updated"), None).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(updated.command, "echo updated");
        assert_eq!(updated.id, job.id);
    }

    #[test]
    fn update_changes_expression_via_handler() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", None, "echo test");

        run_update(&config, &job.id, Some("0 9 * * *"), None, None, None).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(updated.expression, "0 9 * * *");
    }

    #[test]
    fn update_changes_name_via_handler() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", None, "echo test");

        run_update(&config, &job.id, None, None, None, Some("new-name")).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(updated.name.as_deref(), Some("new-name"));
    }

    #[test]
    fn update_tz_alone_sets_timezone() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", None, "echo test");

        run_update(
            &config,
            &job.id,
            None,
            Some("America/Los_Angeles"),
            None,
            None,
        )
        .unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(
            updated.schedule,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: Some("America/Los_Angeles".into()),
            }
        );
    }

    #[test]
    fn update_expression_preserves_existing_tz() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(
            &config,
            "*/5 * * * *",
            Some("America/Los_Angeles"),
            "echo test",
        );

        run_update(&config, &job.id, Some("0 9 * * *"), None, None, None).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(
            updated.schedule,
            Schedule::Cron {
                expr: "0 9 * * *".into(),
                tz: Some("America/Los_Angeles".into()),
            }
        );
    }

    #[test]
    fn update_preserves_unchanged_fields() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_shell_job(
            &config,
            Some("original-name".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo original",
            None,
            false,
            None,
        )
        .unwrap();

        run_update(&config, &job.id, None, None, Some("echo changed"), None).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(updated.command, "echo changed");
        assert_eq!(updated.name.as_deref(), Some("original-name"));
        assert_eq!(updated.expression, "*/5 * * * *");
    }

    #[test]
    fn update_no_flags_fails() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", None, "echo test");

        let result = run_update(&config, &job.id, None, None, None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("At least one of"));
    }

    #[test]
    fn update_nonexistent_job_fails() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let result = run_update(
            &config,
            "nonexistent-id",
            None,
            None,
            Some("echo test"),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn update_security_allows_safe_command() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        assert!(security.is_command_allowed("echo safe"));
    }

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

    #[test]
    fn cron_add_refused_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.cron.enabled = false;
        let err = handle_command(
            crate::CronCommands::Add {
                expression: "0 9 * * *".into(),
                tz: None,
                command: "echo hi".into(),
                agent: false,
                model: None,
            },
            &config,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("cron.enabled=false"),
            "expected a disabled refusal, got: {err}"
        );
        assert!(
            list_jobs(&config).unwrap().is_empty(),
            "a refused add must persist nothing"
        );
    }

    #[test]
    fn cron_list_allowed_when_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.cron.enabled = false;
        // Reads stay open even when cron is disabled, so an operator can inspect
        // dormant jobs.
        handle_command(crate::CronCommands::List, &config).unwrap();
    }

    #[tokio::test]
    async fn cli_run_reports_and_records() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_shell_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo cli-run",
            None,
            false,
            None,
        )
        .unwrap();
        let out = run_job_report(&config, &job.id).await.unwrap();
        assert!(out.contains("ok") || out.contains("cli-run"), "{out}");
        assert_eq!(list_runs(&config, &job.id, 10).unwrap().len(), 1);
    }

    #[test]
    fn cli_runs_lists_history() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_shell_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo x",
            None,
            false,
            None,
        )
        .unwrap();
        record_run(&config, &job.id, Utc::now(), Utc::now(), "ok", Some("x"), 3).unwrap();
        let text = runs_report(&config, &job.id, 10).unwrap();
        assert!(text.contains(&job.id) || text.contains("ok"), "{text}");
    }

    #[test]
    fn parse_delay_rejects_oversized_amount_without_panicking() {
        // A value that overflows chrono::Duration for the unit must be an Err,
        // not a panic. (Days are the tightest bound.)
        let err = parse_delay("999999999999d").unwrap_err();
        assert!(
            err.to_string().contains("delay too large")
                || err.to_string().contains("number too large")
                || err.to_string().contains("invalid digit"),
            "expected a bounded error, got: {err}"
        );
    }

    #[test]
    fn parse_delay_accepts_reasonable_values() {
        assert_eq!(parse_delay("30").unwrap(), chrono::Duration::minutes(30));
        assert_eq!(parse_delay("2h").unwrap(), chrono::Duration::hours(2));
        assert_eq!(parse_delay("45s").unwrap(), chrono::Duration::seconds(45));
        assert_eq!(parse_delay("7d").unwrap(), chrono::Duration::days(7));
    }

    #[test]
    fn add_once_errors_on_overflowing_delay_instead_of_panicking() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        // Large-but-parses-as-i64 value; must surface as Err, never panic/abort.
        let result = add_once(&config, "9999999999d", "echo x");
        assert!(result.is_err(), "oversized delay must return Err");
    }
}
