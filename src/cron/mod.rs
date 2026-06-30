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
    add_agent_job, add_job, add_shell_job, add_shell_job_with_delivery, due_jobs, get_job,
    list_jobs, list_runs, record_last_run, record_run, remove_job, reschedule_after_run,
    update_job,
};
pub use types::{CronJob, CronJobPatch, CronRun, DeliveryConfig, JobType, Schedule, SessionTarget};

/// Build a [`DeliveryConfig`] from `cron add` CLI delivery flags.
///
/// `--announce --channel X --to Y` → announce mode; `--webhook URL` → webhook
/// mode; neither → no delivery. The two modes are mutually exclusive.
fn delivery_from_flags(
    announce: bool,
    channel: Option<String>,
    to: Option<String>,
    webhook: Option<String>,
) -> Result<Option<DeliveryConfig>> {
    match (announce, webhook) {
        (false, None) => Ok(None),
        (true, Some(_)) => bail!("--announce and --webhook are mutually exclusive"),
        (false, Some(url)) => Ok(Some(DeliveryConfig {
            mode: "webhook".to_string(),
            channel: None,
            to: Some(url),
            best_effort: true,
        })),
        (true, None) => {
            let channel =
                channel.ok_or_else(|| anyhow::anyhow!("--announce requires --channel"))?;
            let to = to.ok_or_else(|| anyhow::anyhow!("--announce requires --to"))?;
            Ok(Some(DeliveryConfig {
                mode: "announce".to_string(),
                channel: Some(channel),
                to: Some(to),
                best_effort: true,
            }))
        }
    }
}

/// Print a one-line delivery summary for a freshly created job, if configured.
fn print_delivery(job: &CronJob) {
    let d = &job.delivery;
    if d.mode.is_empty() || d.mode.eq_ignore_ascii_case("none") {
        return;
    }
    if d.mode.eq_ignore_ascii_case("webhook") {
        println!("  Deliver: webhook → {}", d.to.as_deref().unwrap_or("-"));
    } else {
        println!(
            "  Deliver: {} → {} ({})",
            d.mode,
            d.channel.as_deref().unwrap_or("-"),
            d.to.as_deref().unwrap_or("-")
        );
    }
}

#[allow(clippy::needless_pass_by_value)]
pub fn handle_command(command: crate::CronCommands, config: &Config) -> Result<()> {
    match command {
        crate::CronCommands::List => {
            let jobs = list_jobs(config)?;
            if jobs.is_empty() {
                println!("No scheduled tasks yet.");
                println!("\nUsage:");
                println!("  rantaiclaw cron add '0 9 * * *' 'agent -m \"Good morning!\"'");
                return Ok(());
            }

            println!("🕒 Scheduled jobs ({}):", jobs.len());
            for job in jobs {
                let last_run = job
                    .last_run
                    .map_or_else(|| "never".into(), |d| d.to_rfc3339());
                let last_status = job.last_status.unwrap_or_else(|| "n/a".into());
                println!(
                    "- {} | {:?} | next={} | last={} ({})",
                    job.id,
                    job.schedule,
                    job.next_run.to_rfc3339(),
                    last_run,
                    last_status,
                );
                if !job.command.is_empty() {
                    println!("    cmd: {}", job.command);
                }
                if let Some(prompt) = &job.prompt {
                    println!("    prompt: {prompt}");
                }
            }
            Ok(())
        }
        crate::CronCommands::Add {
            expression,
            tz,
            command,
            announce,
            channel,
            to,
            webhook,
        } => {
            let delivery = delivery_from_flags(announce, channel, to, webhook)?;
            let schedule = Schedule::Cron {
                expr: expression,
                tz,
            };
            let job = add_shell_job_with_delivery(config, None, schedule, &command, delivery)?;
            println!("✅ Added cron job {}", job.id);
            println!("  Expr: {}", job.expression);
            println!("  Next: {}", job.next_run.to_rfc3339());
            println!("  Cmd : {}", job.command);
            print_delivery(&job);
            Ok(())
        }
        crate::CronCommands::AddAt {
            at,
            command,
            announce,
            channel,
            to,
            webhook,
        } => {
            let delivery = delivery_from_flags(announce, channel, to, webhook)?;
            let at = chrono::DateTime::parse_from_rfc3339(&at)
                .map_err(|e| anyhow::anyhow!("Invalid RFC3339 timestamp for --at: {e}"))?
                .with_timezone(&chrono::Utc);
            let schedule = Schedule::At { at };
            let job = add_shell_job_with_delivery(config, None, schedule, &command, delivery)?;
            println!("✅ Added one-shot cron job {}", job.id);
            println!("  At  : {}", job.next_run.to_rfc3339());
            println!("  Cmd : {}", job.command);
            print_delivery(&job);
            Ok(())
        }
        crate::CronCommands::AddEvery {
            every_ms,
            command,
            announce,
            channel,
            to,
            webhook,
        } => {
            let delivery = delivery_from_flags(announce, channel, to, webhook)?;
            let schedule = Schedule::Every { every_ms };
            let job = add_shell_job_with_delivery(config, None, schedule, &command, delivery)?;
            println!("✅ Added interval cron job {}", job.id);
            println!("  Every(ms): {every_ms}");
            println!("  Next     : {}", job.next_run.to_rfc3339());
            println!("  Cmd      : {}", job.command);
            print_delivery(&job);
            Ok(())
        }
        crate::CronCommands::Once {
            delay,
            command,
            announce,
            channel,
            to,
            webhook,
        } => {
            let delivery = delivery_from_flags(announce, channel, to, webhook)?;
            let duration = parse_delay(&delay)?;
            let at = chrono::Utc::now() + duration;
            let schedule = Schedule::At { at };
            let job = add_shell_job_with_delivery(config, None, schedule, &command, delivery)?;
            println!("✅ Added one-shot cron job {}", job.id);
            println!("  At  : {}", job.next_run.to_rfc3339());
            println!("  Cmd : {}", job.command);
            print_delivery(&job);
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
            println!("  Cmd : {}", job.command);
            Ok(())
        }
        crate::CronCommands::Remove { id } => remove_job(config, &id),
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
    }
}

pub fn add_once(config: &Config, delay: &str, command: &str) -> Result<CronJob> {
    let duration = parse_delay(delay)?;
    let at = chrono::Utc::now() + duration;
    add_once_at(config, at, command)
}

pub fn add_once_at(
    config: &Config,
    at: chrono::DateTime<chrono::Utc>,
    command: &str,
) -> Result<CronJob> {
    let schedule = Schedule::At { at };
    add_shell_job(config, None, schedule, command)
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
    let duration = match unit {
        "s" => chrono::Duration::seconds(amount),
        "m" => chrono::Duration::minutes(amount),
        "h" => chrono::Duration::hours(amount),
        "d" => chrono::Duration::days(amount),
        _ => anyhow::bail!("unsupported delay unit '{unit}', use s/m/h/d"),
    };
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn delivery_flags_none_yields_no_delivery() {
        assert!(delivery_from_flags(false, None, None, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn delivery_flags_announce_builds_announce_config() {
        let cfg = delivery_from_flags(true, Some("whatsapp".into()), Some("+62".into()), None)
            .unwrap()
            .expect("announce should produce a delivery config");
        assert_eq!(cfg.mode, "announce");
        assert_eq!(cfg.channel.as_deref(), Some("whatsapp"));
        assert_eq!(cfg.to.as_deref(), Some("+62"));
    }

    #[test]
    fn delivery_flags_announce_requires_channel_and_to() {
        assert!(delivery_from_flags(true, None, Some("+62".into()), None).is_err());
        assert!(delivery_from_flags(true, Some("whatsapp".into()), None, None).is_err());
    }

    #[test]
    fn delivery_flags_webhook_builds_webhook_config() {
        let cfg = delivery_from_flags(false, None, None, Some("https://h/x".into()))
            .unwrap()
            .expect("webhook should produce a delivery config");
        assert_eq!(cfg.mode, "webhook");
        assert_eq!(cfg.to.as_deref(), Some("https://h/x"));
    }

    #[test]
    fn delivery_flags_announce_and_webhook_conflict() {
        assert!(delivery_from_flags(
            true,
            Some("whatsapp".into()),
            Some("+62".into()),
            Some("https://h".into()),
        )
        .is_err());
    }
}
