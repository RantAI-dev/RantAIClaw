use crate::config::Config;
use crate::cron::{
    next_run_for_schedule, schedule_cron_expression, validate_schedule, CronJob, CronJobPatch,
    CronRun, DeliveryConfig, JobType, Schedule, SessionTarget, CRON_DISABLED_MESSAGE,
    MIN_JOB_ID_PREFIX_LEN,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::types::{FromSqlResult, ValueRef};
use rusqlite::{params, Connection};
use uuid::Uuid;

/// Refuse an operator-facing write while the cron feature switch is off.
///
/// Every door checks the flag too — they give a better message earlier — but
/// this is the line that cannot be skipped: a door that forgets to ask cannot
/// reach the store. The message is byte-identical to the one the doors use.
fn ensure_cron_enabled(config: &Config) -> Result<()> {
    if config.cron.enabled {
        Ok(())
    } else {
        anyhow::bail!("{}", CRON_DISABLED_MESSAGE)
    }
}

/// What a user-supplied job id resolved to.
enum JobIdMatch {
    /// Exactly one job matched — its full id.
    Job(String),
    /// Nothing matched the id or any prefix of it.
    NotFound,
    /// The string was shorter than [`MIN_JOB_ID_PREFIX_LEN`], so it cannot be
    /// used as a prefix.
    TooShort,
    /// Several jobs share the prefix — their full ids and names.
    Ambiguous(Vec<(String, Option<String>)>),
}

/// Resolve a user-supplied job id (a full id, or a prefix of one) to the full
/// id of exactly one job.
///
/// The rules, in order:
///
/// - An **exact** match on the full id always wins, so a full id is never
///   ambiguous even when it is also a prefix of another id.
/// - Otherwise the string is a prefix: it must be at least
///   [`MIN_JOB_ID_PREFIX_LEN`] characters (what every surface prints) and it
///   must match exactly one job id. Several matches are refused with the
///   candidates named; none is the "not found" refusal.
///
/// The prefix is compared with `substr(id, 1, length(?1)) = ?1` — a literal
/// comparison, not `LIKE`, so a `%` or `_` in the input matches nothing
/// instead of everything.
fn resolve_job_id_in_conn(conn: &Connection, job_id: &str) -> Result<JobIdMatch> {
    match conn.query_row(
        "SELECT id FROM cron_jobs WHERE id = ?1",
        params![job_id],
        |row| row.get::<_, String>(0),
    ) {
        Ok(id) => return Ok(JobIdMatch::Job(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => {}
        Err(e) => return Err(e).context("Failed to look up cron job"),
    }

    if job_id.chars().count() < MIN_JOB_ID_PREFIX_LEN {
        return Ok(JobIdMatch::TooShort);
    }

    let mut stmt = conn.prepare(
        "SELECT id, name FROM cron_jobs WHERE substr(id, 1, length(?1)) = ?1 ORDER BY id",
    )?;
    let rows = stmt.query_map(params![job_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    let mut candidates: Vec<(String, Option<String>)> = Vec::new();
    for row in rows {
        candidates.push(row?);
    }
    if candidates.is_empty() {
        return Ok(JobIdMatch::NotFound);
    }
    if candidates.len() == 1 {
        let (id, _name) = candidates.remove(0);
        return Ok(JobIdMatch::Job(id));
    }
    Ok(JobIdMatch::Ambiguous(candidates))
}

/// The refusal for a prefix that names more than one job: every candidate's
/// full id and name, so the operator can pick one.
///
/// The name is sanitized for the terminal: it reaches the operator's screen
/// (CLI, TUI, HTTP body), and a job name can come from a model-created job.
fn ambiguous_job_id_message(input: &str, candidates: &[(String, Option<String>)]) -> String {
    let listed = candidates
        .iter()
        .map(|(id, name)| {
            let label = name
                .as_deref()
                .filter(|n| !n.trim().is_empty())
                .map(crate::memory::sanitize::sanitize_for_terminal)
                .unwrap_or_else(|| "unnamed".to_string());
            format!("'{id}' ({label})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Ambiguous cron job id '{input}': matches {} jobs — {listed}",
        candidates.len()
    )
}

/// Turn a resolution outcome into the full job id, or into the refusal the
/// operator sees.
///
/// The "not found" wording is unchanged (the HTTP API maps it to a 404 by
/// substring); the other two refusals deliberately avoid that wording, so
/// they read as a bad request rather than a missing job.
fn resolved_job_id(input: &str, matched: JobIdMatch) -> Result<String> {
    match matched {
        JobIdMatch::Job(id) => Ok(id),
        JobIdMatch::NotFound => anyhow::bail!("Cron job '{input}' not found"),
        JobIdMatch::TooShort => anyhow::bail!(
            "Cron job id '{input}' is too short: a prefix must be at least \
             {MIN_JOB_ID_PREFIX_LEN} characters"
        ),
        JobIdMatch::Ambiguous(candidates) => {
            anyhow::bail!("{}", ambiguous_job_id_message(input, &candidates))
        }
    }
}

/// Refuse to persist a job whose announce delivery names a locked channel.
/// `deliver_if_configured` (`scheduler.rs`) already refuses this at delivery
/// time; this stops the job from ever being written with a target that can
/// never receive it, on creation and on every later edit.
fn validate_announce_delivery(delivery: &DeliveryConfig) -> Result<()> {
    if !delivery.mode.eq_ignore_ascii_case("announce") {
        return Ok(());
    }
    let Some(channel) = delivery.channel.as_deref() else {
        return Ok(());
    };
    let key = channel.to_ascii_lowercase();
    if let Some(locked_key) = crate::channels::locked_channel_key_for_provisioner(&key) {
        anyhow::bail!(
            "delivery.channel \"{locked_key}\" is {} and cannot receive scheduled announcements",
            crate::channels::ChannelSupport::UnderDevelopment.label()
        );
    }
    Ok(())
}

const MAX_CRON_OUTPUT_BYTES: usize = 16 * 1024;
const TRUNCATED_OUTPUT_MARKER: &str = "\n...[truncated]";

impl rusqlite::types::FromSql for JobType {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let text = value.as_str()?;
        JobType::try_from(text).map_err(|e| rusqlite::types::FromSqlError::Other(e.into()))
    }
}

pub fn add_job(config: &Config, expression: &str, command: &str) -> Result<CronJob> {
    let schedule = Schedule::Cron {
        expr: expression.to_string(),
        tz: None,
    };
    add_shell_job(
        config, None, schedule, command, None, false, None, None, None, None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn add_shell_job(
    config: &Config,
    name: Option<String>,
    schedule: Schedule,
    command: &str,
    delivery: Option<DeliveryConfig>,
    delete_after_run: bool,
    created_by: Option<&str>,
    origin_channel: Option<&str>,
    origin_chat: Option<&str>,
    origin_thread: Option<&str>,
) -> Result<CronJob> {
    ensure_cron_enabled(config)?;
    let now = Utc::now();
    validate_schedule(&schedule, now)?;
    let next_run = next_run_for_schedule(&schedule, now)?;
    let id = Uuid::new_v4().to_string();
    let expression = schedule_cron_expression(&schedule).unwrap_or_default();
    let schedule_json = serde_json::to_string(&schedule)?;
    let delivery = delivery.unwrap_or_default();
    validate_announce_delivery(&delivery)?;

    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (
                id, expression, command, schedule, job_type, prompt, name, session_target, model,
                enabled, delivery, delete_after_run, created_at, next_run, created_by,
                origin_channel, origin_chat, origin_thread
             ) VALUES (?1, ?2, ?3, ?4, 'shell', NULL, ?5, 'isolated', NULL, 1, ?6, ?7, ?8, ?9, ?10,
                       ?11, ?12, ?13)",
            params![
                id,
                expression,
                command,
                schedule_json,
                name,
                serde_json::to_string(&delivery)?,
                if delete_after_run { 1 } else { 0 },
                now.to_rfc3339(),
                next_run.to_rfc3339(),
                created_by,
                origin_channel,
                origin_chat,
                origin_thread,
            ],
        )
        .context("Failed to insert cron shell job")?;
        Ok(())
    })?;

    get_job(config, &id)
}

#[allow(clippy::too_many_arguments)]
pub fn add_agent_job(
    config: &Config,
    name: Option<String>,
    schedule: Schedule,
    prompt: &str,
    session_target: SessionTarget,
    model: Option<String>,
    delivery: Option<DeliveryConfig>,
    delete_after_run: bool,
    created_by: Option<&str>,
    origin_channel: Option<&str>,
    origin_chat: Option<&str>,
    origin_thread: Option<&str>,
) -> Result<CronJob> {
    ensure_cron_enabled(config)?;
    let now = Utc::now();
    validate_schedule(&schedule, now)?;
    let next_run = next_run_for_schedule(&schedule, now)?;
    let id = Uuid::new_v4().to_string();
    let expression = schedule_cron_expression(&schedule).unwrap_or_default();
    let schedule_json = serde_json::to_string(&schedule)?;
    let delivery = delivery.unwrap_or_default();
    validate_announce_delivery(&delivery)?;

    with_connection(config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (
                id, expression, command, schedule, job_type, prompt, name, session_target, model,
                enabled, delivery, delete_after_run, created_at, next_run, created_by,
                origin_channel, origin_chat, origin_thread
             ) VALUES (?1, ?2, '', ?3, 'agent', ?4, ?5, ?6, ?7, 1, ?8, ?9, ?10, ?11, ?12,
                       ?13, ?14, ?15)",
            params![
                id,
                expression,
                schedule_json,
                prompt,
                name,
                session_target.as_str(),
                model,
                serde_json::to_string(&delivery)?,
                if delete_after_run { 1 } else { 0 },
                now.to_rfc3339(),
                next_run.to_rfc3339(),
                created_by,
                origin_channel,
                origin_chat,
                origin_thread,
            ],
        )
        .context("Failed to insert cron agent job")?;
        Ok(())
    })?;

    get_job(config, &id)
}

pub fn list_jobs(config: &Config) -> Result<Vec<CronJob>> {
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output, created_by, origin_channel, origin_chat, origin_thread
             FROM cron_jobs ORDER BY next_run ASC",
        )?;

        let rows = stmt.query_map([], map_cron_job_row)?;

        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row?);
        }
        Ok(jobs)
    })
}

/// List jobs visible from a given chat. The TUI, CLI, and web console pass
/// `None` and see every job; a chat passes `Some((channel, chat, thread))`
/// and sees only the jobs it created. The `thread` is the caller's current
/// thread id — `None` for a top-level chat. A job is visible when its
/// `origin_channel`, `origin_chat`, and `origin_thread` all match the
/// caller's. A chat that targets a thread does NOT see jobs created in a
/// different thread of the same chat, because the per-job view scopes memory
/// by thread and the user expects "my thread" to mean "this thread".
///
/// Origin-less jobs (CLI / TUI / web console / a path that did not set them)
/// are managed only from those surfaces — a chat neither lists nor changes
/// them. Legacy rows written before the `origin_thread` column existed have
/// `origin_thread = NULL` and so match the top-level `thread = None` query,
/// not a `Some(thread)` query; that is exactly the pre-fix behaviour.
pub fn list_jobs_for_origin(
    config: &Config,
    origin: Option<(&str, &str, Option<&str>)>,
) -> Result<Vec<CronJob>> {
    let all = list_jobs(config)?;
    Ok(match origin {
        None => all,
        Some((channel, chat, thread)) => all
            .into_iter()
            .filter(|job| {
                job.origin_channel.as_deref() == Some(channel)
                    && job.origin_chat.as_deref() == Some(chat)
                    && job.origin_thread.as_deref() == thread
            })
            .collect(),
    })
}

/// `Ok(())` when the caller may see and manage the job; `Err` with the exact
/// sentence a missing job produces, so a chat cannot distinguish "not mine"
/// from "nowhere" — the refusal carries no trace that the job exists.
/// Un-scoped callers (TUI / CLI / console) always pass — they own every job.
pub fn ensure_visible_to_origin(
    job: &CronJob,
    origin: Option<(&str, &str, Option<&str>)>,
) -> Result<(), String> {
    let Some((channel, chat, thread)) = origin else {
        return Ok(());
    };
    match (
        job.origin_channel.as_deref(),
        job.origin_chat.as_deref(),
        job.origin_thread.as_deref(),
    ) {
        (Some(c), Some(h), t) if c == channel && h == chat && t == thread => Ok(()),
        _ => Err(format!("Cron job '{}' not found", job.id)),
    }
}

/// Read one job by its id.
///
/// The id may be the full id or a prefix of it (at least
/// [`MIN_JOB_ID_PREFIX_LEN`] characters) — see [`resolve_job_id_in_conn`].
/// Reads stay open when `cron.enabled` is off, exactly like the CLI's.
pub fn get_job(config: &Config, job_id: &str) -> Result<CronJob> {
    with_connection(config, |conn| {
        let matched = resolve_job_id_in_conn(conn, job_id)?;
        let id = resolved_job_id(job_id, matched)?;
        let mut stmt = conn.prepare(
            "SELECT id, expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output, created_by, origin_channel, origin_chat, origin_thread
             FROM cron_jobs WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            map_cron_job_row(row).map_err(Into::into)
        } else {
            anyhow::bail!("Cron job '{job_id}' not found")
        }
    })
}

/// Delete one job by its id (full id or a prefix of it).
///
/// Gated: a delete is an operator door, so it is refused while `cron.enabled`
/// is off.
pub fn remove_job(config: &Config, id: &str) -> Result<()> {
    ensure_cron_enabled(config)?;
    remove_job_ungated(config, id)
}

/// The scheduler's own delete: a fired one-shot that asked to be removed
/// deletes itself.
///
/// Exempt from the `cron.enabled` gate — it is bookkeeping for a run that
/// already happened, not an operator door.
pub(crate) fn remove_job_by_scheduler(config: &Config, id: &str) -> Result<()> {
    remove_job_ungated(config, id)
}

fn remove_job_ungated(config: &Config, id: &str) -> Result<()> {
    with_connection(config, |conn| {
        let matched = resolve_job_id_in_conn(conn, id)?;
        let resolved = resolved_job_id(id, matched)?;
        let changed = conn
            .execute("DELETE FROM cron_jobs WHERE id = ?1", params![resolved])
            .context("Failed to delete cron job")?;
        if changed == 0 {
            anyhow::bail!("Cron job '{id}' not found");
        }
        Ok(())
    })
}

pub fn due_jobs(config: &Config, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
    let lim = i64::try_from(config.scheduler.max_tasks.max(1))
        .context("Scheduler max_tasks overflows i64")?;
    with_connection(config, |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, expression, command, schedule, job_type, prompt, name, session_target, model,
                    enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output, created_by, origin_channel, origin_chat, origin_thread
             FROM cron_jobs
             WHERE enabled = 1 AND next_run <= ?1
             ORDER BY next_run ASC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![now.to_rfc3339(), lim], map_cron_job_row)?;

        let mut jobs = Vec::new();
        for row in rows {
            jobs.push(row?);
        }
        Ok(jobs)
    })
}

/// A due run is "stale" when its scheduled instant is older than `max_age_secs`.
/// `0` disables the gate (never stale) — a due job then always fires once.
pub fn is_run_stale(next_run: DateTime<Utc>, now: DateTime<Utc>, max_age_secs: u64) -> bool {
    if max_age_secs == 0 {
        return false;
    }
    let Ok(secs) = i64::try_from(max_age_secs) else {
        return false;
    };
    match now.checked_sub_signed(chrono::Duration::seconds(secs)) {
        Some(cutoff) => next_run < cutoff,
        None => false,
    }
}

/// Overwrite only the `next_run` column (no run-history / last_* side effects).
pub fn set_next_run(config: &Config, job_id: &str, next_run: DateTime<Utc>) -> Result<()> {
    with_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET next_run = ?1 WHERE id = ?2",
            params![next_run.to_rfc3339(), job_id],
        )
        .context("Failed to set cron next_run")?;
        Ok(())
    })
}

/// Skip a stale due job WITHOUT running it: a recurring schedule re-anchors to
/// its next future occurrence (and stays enabled — never silently disable a
/// recurring job on a long outage); a one-shot `At` is disabled (a missed
/// reminder must not fire late).
///
/// Bookkeeping for a run that already came due — exempt from the
/// `cron.enabled` gate, like every other scheduler write.
pub fn skip_stale_run(config: &Config, job: &CronJob, now: DateTime<Utc>) -> Result<()> {
    match job.schedule {
        Schedule::At { .. } => {
            update_job_by_scheduler(
                config,
                &job.id,
                CronJobPatch {
                    enabled: Some(false),
                    ..CronJobPatch::default()
                },
            )?;
        }
        _ => {
            let next_run = next_run_for_schedule(&job.schedule, now)?;
            set_next_run(config, &job.id, next_run)?;
        }
    }
    Ok(())
}

/// Edit one job by its id (full id or a prefix of it).
///
/// Gated: an edit is an operator door, so it is refused while `cron.enabled`
/// is off.
pub fn update_job(config: &Config, job_id: &str, patch: CronJobPatch) -> Result<CronJob> {
    ensure_cron_enabled(config)?;
    update_job_ungated(config, job_id, patch)
}

/// The scheduler's own edit: a fired one-shot that stays in the table is
/// disabled so the poller cannot re-fire it.
///
/// Exempt from the `cron.enabled` gate — it records what already ran, and the
/// daemon decides ONCE at startup whether the scheduler runs at all. Refusing
/// it here would leave a fired one-shot enabled and it would fire forever.
pub(crate) fn update_job_by_scheduler(
    config: &Config,
    job_id: &str,
    patch: CronJobPatch,
) -> Result<CronJob> {
    update_job_ungated(config, job_id, patch)
}

/// The body of [`update_job`], without the `cron.enabled` gate.
///
/// The gate lives in the callers that are operator doors; this one is the
/// shared body the scheduler's own writes use.
fn update_job_ungated(config: &Config, job_id: &str, patch: CronJobPatch) -> Result<CronJob> {
    let resolved = with_connection(config, |conn| {
        // Resolve the id (a full id, or a prefix of one) before the write
        // transaction opens.
        let matched = resolve_job_id_in_conn(conn, job_id)?;
        resolved_job_id(job_id, matched)
    })?;

    with_connection(config, |conn| {
        // IMMEDIATE takes the write lock up front so a concurrent
        // reschedule_after_run cannot commit between our read and our write —
        // that race is exactly how an operator edit used to clobber a
        // scheduler-written next_run.
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;

        // Read the current row inside the write transaction. Scoped in a block so
        // the prepared statement/cursor are dropped before we UPDATE.
        let mut job = {
            let mut stmt = tx.prepare(
                "SELECT id, expression, command, schedule, job_type, prompt, name, session_target, model,
                        enabled, delivery, delete_after_run, created_at, next_run, last_run, last_status, last_output, created_by, origin_channel, origin_chat, origin_thread
                 FROM cron_jobs WHERE id = ?1",
            )?;
            let mut rows = stmt.query(params![resolved])?;
            match rows.next()? {
                Some(row) => map_cron_job_row(row).map_err(anyhow::Error::from)?,
                None => anyhow::bail!("Cron job '{job_id}' not found"),
            }
        };

        let mut schedule_changed = false;
        if let Some(schedule) = patch.schedule {
            validate_schedule(&schedule, Utc::now())?;
            job.schedule = schedule;
            job.expression = schedule_cron_expression(&job.schedule).unwrap_or_default();
            schedule_changed = true;
        }
        if let Some(command) = patch.command {
            job.command = command;
        }
        if let Some(prompt) = patch.prompt {
            job.prompt = Some(prompt);
        }
        if let Some(name) = patch.name {
            job.name = Some(name);
        }
        if let Some(enabled) = patch.enabled {
            job.enabled = enabled;
        }
        if let Some(delivery) = patch.delivery {
            validate_announce_delivery(&delivery)?;
            job.delivery = delivery;
        }
        if let Some(model) = patch.model {
            job.model = Some(model);
        }
        if let Some(target) = patch.session_target {
            job.session_target = target;
        }
        if let Some(delete_after_run) = patch.delete_after_run {
            job.delete_after_run = delete_after_run;
        }

        if schedule_changed {
            job.next_run = next_run_for_schedule(&job.schedule, Utc::now())?;
        }

        // A fired one-shot (`Schedule::At`) is disabled but keeps its now-past
        // `at` as next_run. Re-enabling it (resume / PUT {enabled:true} / TUI `p`)
        // would make it due on the next poll tick and silently re-run a job whose
        // contract is "runs once". Refuse unless the same patch also supplies a
        // fresh, future `at` (which reschedules via schedule_changed above).
        if patch.enabled == Some(true) {
            if let Schedule::At { at } = job.schedule {
                if at <= Utc::now() {
                    anyhow::bail!(
                        "cannot re-enable one-shot cron job '{job_id}': its scheduled \
                         time ({at}) is in the past. Supply a new future 'at' to \
                         reschedule it, or create a new job."
                    );
                }
            }
        }

        let schedule_json = serde_json::to_string(&job.schedule)?;
        let job_type_str = <JobType as Into<&str>>::into(job.job_type).to_string();
        let delivery_json = serde_json::to_string(&job.delivery)?;
        let enabled_int = if job.enabled { 1 } else { 0 };
        let delete_after_int = if job.delete_after_run { 1 } else { 0 };

        if schedule_changed {
            // Only a schedule edit is allowed to move next_run.
            tx.execute(
                "UPDATE cron_jobs
                 SET expression = ?1, command = ?2, schedule = ?3, job_type = ?4, prompt = ?5, name = ?6,
                     session_target = ?7, model = ?8, enabled = ?9, delivery = ?10, delete_after_run = ?11,
                     next_run = ?12
                 WHERE id = ?13",
                params![
                    job.expression,
                    job.command,
                    schedule_json,
                    job_type_str,
                    job.prompt,
                    job.name,
                    job.session_target.as_str(),
                    job.model,
                    enabled_int,
                    delivery_json,
                    delete_after_int,
                    job.next_run.to_rfc3339(),
                    job.id,
                ],
            )
            .context("Failed to update cron job")?;
        } else {
            // Leave next_run untouched so an operator edit can never clobber a
            // next_run the scheduler wrote.
            tx.execute(
                "UPDATE cron_jobs
                 SET expression = ?1, command = ?2, schedule = ?3, job_type = ?4, prompt = ?5, name = ?6,
                     session_target = ?7, model = ?8, enabled = ?9, delivery = ?10, delete_after_run = ?11
                 WHERE id = ?12",
                params![
                    job.expression,
                    job.command,
                    schedule_json,
                    job_type_str,
                    job.prompt,
                    job.name,
                    job.session_target.as_str(),
                    job.model,
                    enabled_int,
                    delivery_json,
                    delete_after_int,
                    job.id,
                ],
            )
            .context("Failed to update cron job")?;
        }

        tx.commit().context("Failed to commit cron update")?;
        Ok(())
    })?;

    get_job(config, &resolved)
}

pub fn record_last_run(
    config: &Config,
    job_id: &str,
    finished_at: DateTime<Utc>,
    success: bool,
    output: &str,
) -> Result<()> {
    let status = if success { "ok" } else { "error" };
    let bounded_output = truncate_cron_output(output);
    with_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs
             SET last_run = ?1, last_status = ?2, last_output = ?3
             WHERE id = ?4",
            params![finished_at.to_rfc3339(), status, bounded_output, job_id],
        )
        .context("Failed to update cron last run fields")?;
        Ok(())
    })
}

pub fn reschedule_after_run(
    config: &Config,
    job: &CronJob,
    success: bool,
    output: &str,
) -> Result<()> {
    let now = Utc::now();
    let next_run = next_run_for_schedule(&job.schedule, now)?;
    let status = if success { "ok" } else { "error" };
    let bounded_output = truncate_cron_output(output);

    with_connection(config, |conn| {
        conn.execute(
            "UPDATE cron_jobs
             SET next_run = ?1, last_run = ?2, last_status = ?3, last_output = ?4
             WHERE id = ?5",
            params![
                next_run.to_rfc3339(),
                now.to_rfc3339(),
                status,
                bounded_output,
                job.id
            ],
        )
        .context("Failed to update cron job run state")?;
        Ok(())
    })
}

pub fn record_run(
    config: &Config,
    job_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
) -> Result<()> {
    // A single-execution row is attempt 1.
    record_run_attempt(
        config,
        job_id,
        started_at,
        finished_at,
        status,
        output,
        duration_ms,
        1,
    )
}

/// Like [`record_run`] but stamps the 1-based retry `attempt` index. The
/// scheduler records each retry of a job as its own history row so operators
/// see the real `error, error, ok` sequence and each `duration_ms` reflects
/// that attempt's execution (not the whole retry window including backoff).
#[allow(clippy::too_many_arguments)]
pub fn record_run_attempt(
    config: &Config,
    job_id: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    status: &str,
    output: Option<&str>,
    duration_ms: i64,
    attempt: u32,
) -> Result<()> {
    let bounded_output = output.map(prepare_cron_output);
    with_connection(config, |conn| {
        // Wrap INSERT + pruning DELETE in an explicit transaction so that
        // if the DELETE fails, the INSERT is rolled back and the run table
        // cannot grow unboundedly. IMMEDIATE (not the default DEFERRED): this
        // reads then writes, and two concurrent DEFERRED read→write
        // transactions deadlock with a SQLITE_BUSY that busy_timeout cannot
        // resolve. See sessions/store.rs.
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;

        tx.execute(
            "INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms, attempt)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                job_id,
                started_at.to_rfc3339(),
                finished_at.to_rfc3339(),
                status,
                bounded_output.as_deref(),
                duration_ms,
                i64::from(attempt),
            ],
        )
        .context("Failed to insert cron run")?;

        let keep = i64::from(config.cron.max_run_history.max(1));
        tx.execute(
            "DELETE FROM cron_runs
             WHERE job_id = ?1
               AND id NOT IN (
                 SELECT id FROM cron_runs
                 WHERE job_id = ?1
                 ORDER BY started_at DESC, id DESC
                 LIMIT ?2
               )",
            params![job_id, keep],
        )
        .context("Failed to prune cron run history")?;

        tx.commit()
            .context("Failed to commit cron run transaction")?;
        Ok(())
    })
}

/// Prepare a cron job's output for anywhere it leaves the process.
///
/// Scrub credential-shaped substrings (`api_key=…`, `password:…`) BEFORE
/// truncating, then bound the length. Reuses the agent path's shared scrubber
/// (plan 175) rather than starting a second pattern set.
///
/// Both sinks use this. Run history is a world-readable payload sink served
/// back over the API — and the announcement channel may be a group chat, a
/// louder audience still. Protecting only the quieter path is how the raw
/// output reached chat while the stored copy was clean.
pub(crate) fn prepare_cron_output(output: &str) -> String {
    truncate_cron_output(&crate::agent::loop_::scrub_credentials(output))
}

fn truncate_cron_output(output: &str) -> String {
    if output.len() <= MAX_CRON_OUTPUT_BYTES {
        return output.to_string();
    }

    if MAX_CRON_OUTPUT_BYTES <= TRUNCATED_OUTPUT_MARKER.len() {
        return TRUNCATED_OUTPUT_MARKER.to_string();
    }

    let mut cutoff = MAX_CRON_OUTPUT_BYTES - TRUNCATED_OUTPUT_MARKER.len();
    while cutoff > 0 && !output.is_char_boundary(cutoff) {
        cutoff -= 1;
    }

    let mut truncated = output[..cutoff].to_string();
    truncated.push_str(TRUNCATED_OUTPUT_MARKER);
    truncated
}

/// The run history of one job, newest first.
///
/// The id may be the full id or a prefix of it. An id that names no job has
/// no history, so the answer is an empty list (as it always was); a prefix
/// that names several jobs is refused, because silently returning the wrong
/// job's history would be worse than an error.
pub fn list_runs(config: &Config, job_id: &str, limit: usize) -> Result<Vec<CronRun>> {
    with_connection(config, |conn| {
        let matched = resolve_job_id_in_conn(conn, job_id)?;
        let resolved = match matched {
            JobIdMatch::Job(id) => id,
            JobIdMatch::NotFound | JobIdMatch::TooShort => return Ok(Vec::new()),
            JobIdMatch::Ambiguous(candidates) => {
                anyhow::bail!("{}", ambiguous_job_id_message(job_id, &candidates))
            }
        };
        let lim = i64::try_from(limit.max(1)).context("Run history limit overflow")?;
        let mut stmt = conn.prepare(
            "SELECT id, job_id, started_at, finished_at, status, output, duration_ms, attempt
             FROM cron_runs
             WHERE job_id = ?1
             ORDER BY started_at DESC, id DESC
             LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![resolved, lim], |row| {
            Ok(CronRun {
                id: row.get(0)?,
                job_id: row.get(1)?,
                started_at: parse_rfc3339(&row.get::<_, String>(2)?)
                    .map_err(sql_conversion_error)?,
                finished_at: parse_rfc3339(&row.get::<_, String>(3)?)
                    .map_err(sql_conversion_error)?,
                status: row.get(4)?,
                output: row.get(5)?,
                duration_ms: row.get(6)?,
                attempt: row.get(7)?,
            })
        })?;

        let mut runs = Vec::new();
        for row in rows {
            runs.push(row?);
        }
        Ok(runs)
    })
}

fn parse_rfc3339(raw: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("Invalid RFC3339 timestamp in cron DB: {raw}"))?;
    Ok(parsed.with_timezone(&Utc))
}

fn sql_conversion_error(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(err.into())
}

fn map_cron_job_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CronJob> {
    let expression: String = row.get(1)?;
    let schedule_raw: Option<String> = row.get(3)?;
    let schedule =
        decode_schedule(schedule_raw.as_deref(), &expression).map_err(sql_conversion_error)?;

    let delivery_raw: Option<String> = row.get(10)?;
    let delivery = decode_delivery(delivery_raw.as_deref()).map_err(sql_conversion_error)?;

    let next_run_raw: String = row.get(13)?;
    let last_run_raw: Option<String> = row.get(14)?;
    let created_at_raw: String = row.get(12)?;

    Ok(CronJob {
        id: row.get(0)?,
        expression,
        schedule,
        command: row.get(2)?,
        job_type: row.get(4)?,
        prompt: row.get(5)?,
        name: row.get(6)?,
        session_target: SessionTarget::parse(&row.get::<_, String>(7)?),
        model: row.get(8)?,
        enabled: row.get::<_, i64>(9)? != 0,
        delivery,
        delete_after_run: row.get::<_, i64>(11)? != 0,
        created_at: parse_rfc3339(&created_at_raw).map_err(sql_conversion_error)?,
        next_run: parse_rfc3339(&next_run_raw).map_err(sql_conversion_error)?,
        last_run: match last_run_raw {
            Some(raw) => Some(parse_rfc3339(&raw).map_err(sql_conversion_error)?),
            None => None,
        },
        last_status: row.get(15)?,
        last_output: row.get(16)?,
        created_by: row.get(17)?,
        origin_channel: row.get(18)?,
        origin_chat: row.get(19)?,
        origin_thread: row.get(20)?,
    })
}

fn decode_schedule(schedule_raw: Option<&str>, expression: &str) -> Result<Schedule> {
    if let Some(raw) = schedule_raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return serde_json::from_str(trimmed)
                .with_context(|| format!("Failed to parse cron schedule JSON: {trimmed}"));
        }
    }

    if expression.trim().is_empty() {
        anyhow::bail!("Missing schedule and legacy expression for cron job")
    }

    Ok(Schedule::Cron {
        expr: expression.to_string(),
        tz: None,
    })
}

fn decode_delivery(delivery_raw: Option<&str>) -> Result<DeliveryConfig> {
    if let Some(raw) = delivery_raw {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return serde_json::from_str(trimmed)
                .with_context(|| format!("Failed to parse cron delivery JSON: {trimmed}"));
        }
    }
    Ok(DeliveryConfig::default())
}

fn add_column_if_missing(conn: &Connection, table: &str, name: &str, sql_type: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let col_name: String = row.get(1)?;
        if col_name == name {
            return Ok(());
        }
    }
    // Drop the statement/rows before executing ALTER to release any locks
    drop(rows);
    drop(stmt);

    // Tolerate "duplicate column name" errors to handle the race where
    // another process adds the column between our PRAGMA check and ALTER.
    // `table` is always a hardcoded literal ("cron_jobs"/"cron_runs"), never
    // user input, so interpolating it into the DDL is safe.
    match conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {name} {sql_type}"),
        [],
    ) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(err, Some(ref msg)))
            if msg.contains("duplicate column name") =>
        {
            tracing::debug!("Column {table}.{name} already exists (concurrent migration): {err}");
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("Failed to add {table}.{name}")),
    }
}

/// Best-effort 0600 on the cron DB and its WAL/SHM siblings. Run history can
/// hold command output that passed a secret through a scheduled job's
/// environment, so the file must not be left world-readable at the process
/// umask (commonly 0644). Unix-only, logged-not-fatal — a chmod failure must
/// never stop the scheduler from opening the DB. Mirrors the WhatsApp
/// credential store's `restrict_file_permissions` (plan 175). Idempotent, so
/// running it on every open picks up the WAL siblings once they appear.
#[cfg(unix)]
fn restrict_db_permissions(db_path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    for path in [
        db_path.to_path_buf(),
        db_path.with_extension("db-wal"),
        db_path.with_extension("db-shm"),
    ] {
        if path.exists() {
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            {
                tracing::warn!("cron: could not restrict {} to 0600: {e}", path.display());
            }
        }
    }
}

#[cfg(not(unix))]
fn restrict_db_permissions(_db_path: &std::path::Path) {}

fn with_connection<T>(config: &Config, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let db_path = config.workspace_dir.join("cron").join("jobs.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cron directory: {}", parent.display()))?;
    }

    let conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open cron DB: {}", db_path.display()))?;
    restrict_db_permissions(&db_path);

    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA foreign_keys=ON;
         CREATE TABLE IF NOT EXISTS cron_jobs (
            id               TEXT PRIMARY KEY,
            expression       TEXT NOT NULL,
            command          TEXT NOT NULL,
            schedule         TEXT,
            job_type         TEXT NOT NULL DEFAULT 'shell',
            prompt           TEXT,
            name             TEXT,
            session_target   TEXT NOT NULL DEFAULT 'isolated',
            model            TEXT,
            enabled          INTEGER NOT NULL DEFAULT 1,
            delivery         TEXT,
            delete_after_run INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT NOT NULL,
            next_run         TEXT NOT NULL,
            last_run         TEXT,
            last_status      TEXT,
            last_output      TEXT,
            created_by       TEXT,
            origin_channel   TEXT,
            origin_chat      TEXT,
            origin_thread    TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_cron_jobs_next_run ON cron_jobs(next_run);

        CREATE TABLE IF NOT EXISTS cron_runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id      TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            status      TEXT NOT NULL,
            output      TEXT,
            duration_ms INTEGER,
            attempt     INTEGER NOT NULL DEFAULT 1,
            FOREIGN KEY (job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_cron_runs_job_id ON cron_runs(job_id);
        CREATE INDEX IF NOT EXISTS idx_cron_runs_started_at ON cron_runs(started_at);
        CREATE INDEX IF NOT EXISTS idx_cron_runs_job_started ON cron_runs(job_id, started_at);",
    )
    .context("Failed to initialize cron schema")?;

    migrate_cron_columns_once(&conn, &db_path)?;

    f(&conn)
}

/// Tracks which cron DB paths this process has already migrated, so the
/// `add_column_if_missing` PRAGMA scans run once per DB path — not on every
/// store call (the scheduler calls `due_jobs` every poll tick). A freshly
/// created DB is already fully-formed by the `CREATE TABLE` above; migration
/// only matters for legacy on-disk DBs from an older binary.
static MIGRATED_CRON_DBS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
> = std::sync::OnceLock::new();

fn migrate_cron_columns_once(conn: &Connection, db_path: &std::path::Path) -> Result<()> {
    let set =
        MIGRATED_CRON_DBS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    {
        let guard = set.lock().expect("cron migration lock poisoned");
        if guard.contains(db_path) {
            return Ok(());
        }
    }
    add_column_if_missing(conn, "cron_jobs", "schedule", "TEXT")?;
    add_column_if_missing(
        conn,
        "cron_jobs",
        "job_type",
        "TEXT NOT NULL DEFAULT 'shell'",
    )?;
    add_column_if_missing(conn, "cron_jobs", "prompt", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "name", "TEXT")?;
    add_column_if_missing(
        conn,
        "cron_jobs",
        "session_target",
        "TEXT NOT NULL DEFAULT 'isolated'",
    )?;
    add_column_if_missing(conn, "cron_jobs", "model", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "enabled", "INTEGER NOT NULL DEFAULT 1")?;
    add_column_if_missing(conn, "cron_jobs", "delivery", "TEXT")?;
    add_column_if_missing(
        conn,
        "cron_jobs",
        "delete_after_run",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(conn, "cron_jobs", "created_by", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "origin_channel", "TEXT")?;
    add_column_if_missing(conn, "cron_jobs", "origin_chat", "TEXT")?;
    // `origin_thread` stores the thread id (Slack ts, Telegram forum-topic id
    // encoded into reply_target, …) separately from `origin_chat`. The
    // scheduler rebuilds the per-job view as `ConversationKey::new(channel,
    // chat).in_thread(thread).resolve()` — without the thread column the
    // rebuilt view drops the thread and the job reads a place nothing wrote
    // to. Legacy rows have `NULL`, which means "no thread", exactly matching
    // the pre-fix behaviour.
    add_column_if_missing(conn, "cron_jobs", "origin_thread", "TEXT")?;
    // Backfill the origin from the delivery target when an agent created the
    // job from a chat before this column existed — the spec scopes by the
    // delivery's channel+target, whatever the mode. Other rows (cli / tui /
    // gateway / an agent job without a chat delivery) stay origin-less and
    // are managed only from the TUI / CLI / console.
    let backfilled = conn
        .execute(
            "UPDATE cron_jobs
                SET origin_channel = json_extract(delivery, '$.channel'),
                    origin_chat = json_extract(delivery, '$.to')
              WHERE origin_channel IS NULL
                AND origin_chat IS NULL
                AND created_by = 'agent-tool'
                AND json_extract(delivery, '$.channel') IS NOT NULL
                AND json_extract(delivery, '$.to') IS NOT NULL",
            [],
        )
        .context("Failed to backfill cron job origin columns")?;
    let origin_less: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM cron_jobs WHERE origin_channel IS NULL OR origin_chat IS NULL",
            [],
            |row| row.get(0),
        )
        .context("Failed to count origin-less cron jobs")?;
    // Counts only — never a job name, id or target.
    tracing::info!(
        backfilled,
        origin_less,
        "cron job origin columns migrated; origin-less jobs are managed only from the TUI / CLI / console"
    );
    // Retry attempts are recorded as separate cron_runs rows (plan 185); legacy
    // DBs get the column with DEFAULT 1 so old rows read as attempt 1.
    add_column_if_missing(conn, "cron_runs", "attempt", "INTEGER NOT NULL DEFAULT 1")?;
    set.lock()
        .expect("cron migration lock poisoned")
        .insert(db_path.to_path_buf());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use chrono::Duration as ChronoDuration;
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

    /// Insert a job row with a controlled id.
    ///
    /// The store generates a UUID for every job it creates, so a test that
    /// needs a specific id (two ids sharing a prefix, a full id that is also
    /// a prefix of another) has to write the row itself.
    fn insert_job_with_id(config: &Config, id: &str, name: Option<&str>) {
        let now = Utc::now();
        with_connection(config, |conn| {
            conn.execute(
                "INSERT INTO cron_jobs (
                    id, expression, command, schedule, job_type, name, session_target,
                    enabled, delete_after_run, created_at, next_run
                 ) VALUES (?1, '*/5 * * * *', 'echo ok', NULL, 'shell', ?2, 'isolated', 1, 0, ?3, ?4)",
                params![
                    id,
                    name,
                    now.to_rfc3339(),
                    (now + ChronoDuration::minutes(5)).to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .unwrap();
    }

    /// A prefix that names exactly one job resolves to that job — this is the
    /// whole point of the resolver: the id `cron list` prints must work.
    #[test]
    fn a_unique_id_prefix_resolves_to_its_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        insert_job_with_id(&config, "abcdef0123456789", Some("nightly"));

        let job = get_job(&config, "abcdef01").unwrap();
        assert_eq!(job.id, "abcdef0123456789");
        assert_eq!(job.name.as_deref(), Some("nightly"));
    }

    /// A prefix that names several jobs is refused, and the refusal names
    /// every candidate (full id + name) so the operator can pick one.
    #[test]
    fn an_ambiguous_id_prefix_is_refused_and_names_every_candidate() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        insert_job_with_id(&config, "abcdef01-one", Some("nightly"));
        insert_job_with_id(&config, "abcdef01-two", Some("weekly"));

        let err = get_job(&config, "abcdef01").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("abcdef01-one"), "{msg}");
        assert!(msg.contains("nightly"), "{msg}");
        assert!(msg.contains("abcdef01-two"), "{msg}");
        assert!(msg.contains("weekly"), "{msg}");
        assert!(
            !msg.contains("not found"),
            "an ambiguous id is a bad request, not a missing job: {msg}"
        );
    }

    /// A prefix shorter than the minimum is refused with its own message, and
    /// that message never says "not found" (the HTTP API maps "not found" to
    /// a 404 by substring).
    #[test]
    fn an_id_prefix_below_the_minimum_length_is_refused() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        insert_job_with_id(&config, "abcdef0123456789", Some("nightly"));

        let err = get_job(&config, "abc").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains('8'), "the refusal names the minimum: {msg}");
        assert!(
            !msg.contains("not found"),
            "a too-short id is a bad request, not a missing job: {msg}"
        );
    }

    /// A full id is never ambiguous, even when it is also the prefix of
    /// another job's id.
    #[test]
    fn a_full_id_that_is_also_a_prefix_of_another_resolves_to_itself() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        insert_job_with_id(&config, "abcdef0123456789", Some("first"));
        insert_job_with_id(&config, "abcdef0123456789-extra", Some("second"));

        let job = get_job(&config, "abcdef0123456789").unwrap();
        assert_eq!(job.id, "abcdef0123456789");
        assert_eq!(job.name.as_deref(), Some("first"));
    }

    /// An id that names nothing is still "not found" — the wording the HTTP
    /// API maps to a 404.
    #[test]
    fn an_unknown_id_is_still_not_found() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let err = get_job(&config, "ffffffffffffffff").unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "expected a not-found refusal, got: {err}"
        );
    }

    /// The prefix comparison is literal, not a `LIKE` pattern: a `%` or a `_`
    /// in the input matches nothing, not everything.
    #[test]
    fn a_like_metacharacter_in_an_id_matches_nothing() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        insert_job_with_id(&config, "abcdef0123456789", Some("only"));

        for meta in ["________", "%_______"] {
            let err = get_job(&config, meta).unwrap_err();
            assert!(
                err.to_string().contains("not found"),
                "a LIKE metacharacter must match nothing, got: {err}"
            );
        }
    }

    /// An id that names no job has no history: the answer is an empty list,
    /// not an error. A prefix that names several jobs is refused — silently
    /// returning the wrong job's history would be worse.
    #[test]
    fn list_runs_of_an_unknown_job_is_empty_and_an_ambiguous_prefix_is_refused() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        insert_job_with_id(&config, "abcdef01-one", Some("nightly"));
        insert_job_with_id(&config, "abcdef01-two", Some("weekly"));

        assert!(list_runs(&config, "does-not-exist", 10).unwrap().is_empty());
        assert!(list_runs(&config, "abc", 10).unwrap().is_empty());

        let err = list_runs(&config, "abcdef01", 10).unwrap_err();
        assert!(
            err.to_string().contains("abcdef01-one"),
            "the refusal names the candidates: {err}"
        );
    }

    /// The store gate: a mutating function refuses while `cron.enabled` is
    /// off, even when it is called directly — a door that forgets to ask
    /// cannot reach the store.
    #[test]
    fn mutating_store_functions_refuse_when_cron_is_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        config.cron.enabled = false;

        let shell = add_shell_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo hi",
            None,
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(shell.to_string(), CRON_DISABLED_MESSAGE);

        let agent = add_agent_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "remind",
            SessionTarget::Isolated,
            None,
            None,
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(agent.to_string(), CRON_DISABLED_MESSAGE);

        let edit = update_job(
            &config,
            &job.id,
            CronJobPatch {
                name: Some("renamed".into()),
                ..CronJobPatch::default()
            },
        )
        .unwrap_err();
        assert_eq!(edit.to_string(), CRON_DISABLED_MESSAGE);

        let delete = remove_job(&config, &job.id).unwrap_err();
        assert_eq!(delete.to_string(), CRON_DISABLED_MESSAGE);

        // Nothing was written: the job is untouched.
        let stored = get_job(&config, &job.id).unwrap();
        assert_eq!(stored.name, None);
        assert_eq!(stored.command, "echo ok");
    }

    /// The scheduler's own writes are bookkeeping for a run that already
    /// happened, so they are exempt from the gate. Without the exemption, a
    /// fired one-shot would stay enabled and fire forever.
    #[test]
    fn the_scheduler_bookkeeping_writes_still_land_when_cron_is_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let now = Utc::now();
        config.cron.enabled = false;

        // A stale recurring run re-anchors to the future and stays enabled.
        set_next_run(&config, &job.id, now - ChronoDuration::hours(3)).unwrap();
        let stale = get_job(&config, &job.id).unwrap();
        skip_stale_run(&config, &stale, now).unwrap();
        let after = get_job(&config, &job.id).unwrap();
        assert!(after.enabled, "a recurring job must stay enabled");
        assert!(after.next_run > now, "a recurring job must re-anchor");

        // The run history is still recorded.
        record_run(&config, &job.id, now, now, "ok", Some("ok"), 1).unwrap();
        assert_eq!(list_runs(&config, &job.id, 10).unwrap().len(), 1);

        // The scheduler's own one-shot cleanup still writes.
        update_job_by_scheduler(
            &config,
            &job.id,
            CronJobPatch {
                enabled: Some(false),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        assert!(!get_job(&config, &job.id).unwrap().enabled);
    }

    #[test]
    fn add_job_accepts_five_field_expression() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        assert_eq!(job.expression, "*/5 * * * *");
        assert_eq!(job.command, "echo ok");
        assert!(matches!(job.schedule, Schedule::Cron { .. }));
    }

    #[test]
    fn add_list_remove_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_job(&config, "*/10 * * * *", "echo roundtrip").unwrap();
        let listed = list_jobs(&config).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, job.id);

        remove_job(&config, &job.id).unwrap();
        assert!(list_jobs(&config).unwrap().is_empty());
    }

    #[test]
    fn created_by_round_trips_and_defaults_none() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_shell_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo hi",
            None,
            false,
            Some("cli"),
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(job.created_by.as_deref(), Some("cli"));
        // Survives the round-trip through get_job's row mapper.
        assert_eq!(
            get_job(&config, &job.id).unwrap().created_by.as_deref(),
            Some("cli")
        );

        // A job created without an origin loads as None (legacy rows are NULL).
        let anon = add_shell_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo anon",
            None,
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(anon.created_by, None);
    }

    #[test]
    fn origin_columns_round_trip_and_default_none() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let scoped = add_agent_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "remind",
            SessionTarget::Isolated,
            None,
            None,
            false,
            Some("agent-tool"),
            Some("telegram"),
            Some("chat-a"),
            None,
        )
        .unwrap();
        let loaded = get_job(&config, &scoped.id).unwrap();
        assert_eq!(loaded.origin_channel.as_deref(), Some("telegram"));
        assert_eq!(loaded.origin_chat.as_deref(), Some("chat-a"));
        assert_eq!(loaded.origin_thread, None);

        let unscoped = add_shell_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo ok",
            None,
            false,
            Some("cli"),
            None,
            None,
            None,
        )
        .unwrap();
        let loaded = get_job(&config, &unscoped.id).unwrap();
        assert_eq!(loaded.origin_channel, None);
        assert_eq!(loaded.origin_chat, None);
        assert_eq!(loaded.origin_thread, None);
    }

    /// `origin_thread` carries the thread id from a Slack thread / Telegram
    /// forum topic across the INSERT / SELECT round trip, so the per-job view
    /// rebuilt from those three columns exactly equals the dispatcher's view.
    /// Legacy rows pre-dating the column load with `origin_thread = NULL`,
    /// which the scheduler treats as "no thread" — the same behaviour the
    /// store had before the column existed.
    #[test]
    fn origin_thread_round_trips_and_defaults_none() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let threaded = add_agent_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "remind in thread",
            SessionTarget::Isolated,
            None,
            None,
            false,
            Some("agent-tool"),
            Some("slack"),
            Some("C0CHAN"),
            Some("1700000000.000500"),
        )
        .unwrap();
        let loaded = get_job(&config, &threaded.id).unwrap();
        assert_eq!(loaded.origin_thread.as_deref(), Some("1700000000.000500"));

        // A job with no thread keeps origin_thread = None even when the
        // other two origin columns are set.
        let plain = add_shell_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo plain",
            None,
            false,
            Some("agent-tool"),
            Some("telegram"),
            Some("chat-a"),
            None,
        )
        .unwrap();
        let loaded = get_job(&config, &plain.id).unwrap();
        assert_eq!(loaded.origin_channel.as_deref(), Some("telegram"));
        assert_eq!(loaded.origin_chat.as_deref(), Some("chat-a"));
        assert_eq!(loaded.origin_thread, None);
    }

    /// A legacy agent-tool job whose announce delivery names a chat gets its
    /// origin backfilled from that delivery on the first store open; a TUI job
    /// (or any row without a chat delivery) stays origin-less.
    #[test]
    fn migration_backfills_agent_tool_jobs_from_delivery_and_leaves_the_rest() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        // Seed the shapes the migration has to tell apart, by writing the
        // rows through the normal path and then clearing the origin columns
        // to simulate a DB from a binary that predated the columns.
        let from_chat = add_agent_job(
            &config,
            Some("from-chat".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "remind",
            SessionTarget::Isolated,
            None,
            Some(DeliveryConfig {
                mode: "announce".into(),
                channel: Some("telegram".into()),
                to: Some("legacy-chat".into()),
                best_effort: true,
            }),
            false,
            Some("agent-tool"),
            None,
            None,
            None,
        )
        .unwrap();
        let from_tui = add_shell_job(
            &config,
            Some("from-tui".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo ok",
            None,
            false,
            Some("tui"),
            None,
            None,
            None,
        )
        .unwrap();

        // Simulate the legacy DB: no origin columns filled.
        with_connection(&config, |conn| {
            conn.execute(
                "UPDATE cron_jobs SET origin_channel = NULL, origin_chat = NULL, origin_thread = NULL",
                [],
            )
            .unwrap();
            Ok(())
        })
        .unwrap();
        // Force the one-shot migration to run again for this test's DB.
        MIGRATED_CRON_DBS
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
            .lock()
            .unwrap()
            .remove(&config.workspace_dir.join("cron").join("jobs.db"));

        // Any store call re-runs the migration.
        let _ = list_jobs(&config).unwrap();

        let loaded = get_job(&config, &from_chat.id).unwrap();
        assert_eq!(
            loaded.origin_channel.as_deref(),
            Some("telegram"),
            "an agent-tool job with a chat delivery gets its origin backfilled"
        );
        assert_eq!(loaded.origin_chat.as_deref(), Some("legacy-chat"));

        let loaded = get_job(&config, &from_tui.id).unwrap();
        assert_eq!(loaded.origin_channel, None, "a TUI job stays origin-less");
        assert_eq!(loaded.origin_chat, None);
    }

    #[test]
    fn add_shell_job_persists_delivery() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("telegram".into()),
            to: Some("123".into()),
            best_effort: true,
        };
        let job = add_shell_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo hi",
            Some(delivery),
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        // A shell "run this and message me" job must persist its delivery
        // instead of the old hardcoded DeliveryConfig::default() ("none").
        assert_eq!(job.delivery.mode, "announce");
        assert_eq!(job.delivery.channel.as_deref(), Some("telegram"));
        assert_eq!(job.delivery.to.as_deref(), Some("123"));
    }

    /// A locked channel has a real catalog row, so nothing about `delivery`
    /// deserialization rejects it — the job store itself must refuse it
    /// before a job naming an unreachable channel is ever written.
    #[test]
    fn add_shell_job_refuses_a_locked_delivery_channel() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("mattermost".into()),
            to: Some("123".into()),
            best_effort: true,
        };
        let err = add_shell_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo hi",
            Some(delivery),
            false,
            None,
            None,
            None,
            None,
        )
        .expect_err("a locked delivery channel must be refused at creation");
        assert!(
            err.to_string()
                .contains(crate::channels::ChannelSupport::UnderDevelopment.label()),
            "{err}"
        );
    }

    /// The same refusal applies to a later edit — a job created with no
    /// delivery, or a usable one, cannot be re-pointed at a locked channel
    /// either.
    #[test]
    fn update_job_refuses_re_pointing_delivery_at_a_locked_channel() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_shell_job(
            &config,
            None,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo hi",
            None,
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap();

        let patch = CronJobPatch {
            delivery: Some(DeliveryConfig {
                mode: "announce".into(),
                channel: Some("mattermost".into()),
                to: Some("123".into()),
                best_effort: true,
            }),
            ..Default::default()
        };
        let err = update_job(&config, &job.id, patch)
            .expect_err("re-pointing delivery at a locked channel must be refused");
        assert!(
            err.to_string()
                .contains(crate::channels::ChannelSupport::UnderDevelopment.label()),
            "{err}"
        );

        // The job itself must be untouched by the rejected edit.
        let reread = get_job(&config, &job.id).unwrap();
        assert_eq!(reread.delivery.mode, "none");
    }

    #[test]
    fn is_run_stale_respects_cutoff_and_zero_disables() {
        let now = Utc::now();
        let old = now - ChronoDuration::seconds(7200); // 2h ago
        let recent = now - ChronoDuration::seconds(60); // 1m ago
                                                        // 0 disables the gate → never stale.
        assert!(!is_run_stale(old, now, 0));
        // 1h cutoff: 2h-old is stale, 1m-old is not.
        assert!(is_run_stale(old, now, 3600));
        assert!(!is_run_stale(recent, now, 3600));
    }

    #[test]
    fn skip_stale_run_reanchors_recurring_and_disables_oneshot() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let now = Utc::now();

        // Recurring: force next_run into the past, then skip → re-anchored to the
        // future and still enabled.
        let cron_job = add_job(&config, "*/5 * * * *", "echo hi").unwrap();
        set_next_run(&config, &cron_job.id, now - ChronoDuration::hours(3)).unwrap();
        let stale = get_job(&config, &cron_job.id).unwrap();
        skip_stale_run(&config, &stale, now).unwrap();
        let after = get_job(&config, &cron_job.id).unwrap();
        assert!(after.enabled, "a recurring job must stay enabled");
        assert!(
            after.next_run > now,
            "a recurring job must re-anchor to the future"
        );

        // One-shot At: disabled (a missed reminder must not fire late).
        let at = now + ChronoDuration::hours(1);
        let at_job = add_shell_job(
            &config,
            None,
            Schedule::At { at },
            "echo once",
            None,
            false,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        skip_stale_run(&config, &at_job, now).unwrap();
        let after_at = get_job(&config, &at_job.id).unwrap();
        assert!(!after_at.enabled, "a one-shot At job must be disabled");
    }

    #[test]
    fn due_jobs_filters_by_timestamp_and_enabled() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        // `add_job` schedules the first run strictly after the moment it
        // stamps as `created_at`, so asking at `created_at` itself never
        // selects the new job, however the minute boundary falls. A scheduler
        // that set `next_run` equal to `created_at` would fail both checks.
        let job = add_job(&config, "* * * * *", "echo due").unwrap();
        assert!(job.next_run > job.created_at);

        let due_now = due_jobs(&config, job.created_at).unwrap();
        assert!(due_now.is_empty(), "new job should not be due immediately");

        let far_future = job.created_at + ChronoDuration::days(365);
        let due_future = due_jobs(&config, far_future).unwrap();
        assert_eq!(due_future.len(), 1, "job should be due in far future");

        let _ = update_job(
            &config,
            &job.id,
            CronJobPatch {
                enabled: Some(false),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        let due_after_disable = due_jobs(&config, far_future).unwrap();
        assert!(due_after_disable.is_empty());
    }

    /// A clock reading that lands exactly on, or past, the stored `next_run`
    /// (a minute boundary crossed between creating the job and asking what is
    /// due) selects the job. The clock is injected, so both sides of the
    /// boundary are plain cases rather than a race.
    #[test]
    fn due_jobs_selects_a_job_from_its_next_run_onward() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_job(&config, "* * * * *", "echo due").unwrap();
        let one_second = ChronoDuration::seconds(1);

        let just_before = due_jobs(&config, job.next_run - one_second).unwrap();
        assert!(just_before.is_empty(), "not due before next_run");

        let at_next_run = due_jobs(&config, job.next_run).unwrap();
        assert_eq!(at_next_run.len(), 1, "due exactly at next_run");

        let just_after = due_jobs(&config, job.next_run + one_second).unwrap();
        assert_eq!(just_after.len(), 1, "still due after next_run");
    }

    #[test]
    fn due_jobs_respects_scheduler_max_tasks_limit() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.scheduler.max_tasks = 2;

        let _ = add_job(&config, "* * * * *", "echo due-1").unwrap();
        let _ = add_job(&config, "* * * * *", "echo due-2").unwrap();
        let _ = add_job(&config, "* * * * *", "echo due-3").unwrap();

        let far_future = Utc::now() + ChronoDuration::days(365);
        let due = due_jobs(&config, far_future).unwrap();
        assert_eq!(due.len(), 2);
    }

    #[test]
    fn reschedule_after_run_persists_last_status_and_last_run() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_job(&config, "*/15 * * * *", "echo run").unwrap();
        reschedule_after_run(&config, &job, false, "failed output").unwrap();

        let listed = list_jobs(&config).unwrap();
        let stored = listed.iter().find(|j| j.id == job.id).unwrap();
        assert_eq!(stored.last_status.as_deref(), Some("error"));
        assert!(stored.last_run.is_some());
        assert_eq!(stored.last_output.as_deref(), Some("failed output"));
    }

    // NOTE: the real defect is a two-connection RACE (an operator edit reads
    // next_run on one connection while a reschedule writes it on another). A
    // sequential unit test cannot reproduce that interleaving, so it does NOT
    // fail on the pre-fix code. The primary proof of the fix is structural (the
    // single IMMEDIATE transaction + next_run written only on a schedule change).
    // These two are a sequential sanity check and an over-correction guard.
    #[test]
    fn update_job_without_schedule_change_preserves_next_run() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_job(&config, "*/5 * * * *", "echo hi").unwrap();

        // Simulate a scheduler reschedule writing a fresh next_run.
        reschedule_after_run(&config, &job, true, "ran").unwrap();
        let after_reschedule = get_job(&config, &job.id).unwrap().next_run;

        // An operator edit that does NOT touch the schedule (rename) must not
        // move next_run back.
        update_job(
            &config,
            &job.id,
            CronJobPatch {
                name: Some("renamed".into()),
                ..CronJobPatch::default()
            },
        )
        .unwrap();

        let final_job = get_job(&config, &job.id).unwrap();
        assert_eq!(
            final_job.next_run, after_reschedule,
            "a non-schedule edit must preserve the scheduler-written next_run"
        );
        assert_eq!(final_job.name.as_deref(), Some("renamed"));
    }

    #[test]
    fn update_job_with_schedule_change_recomputes_next_run() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let job = add_job(&config, "*/5 * * * *", "echo hi").unwrap();
        let original_next = job.next_run;

        // A schedule change is the ONLY edit allowed to move next_run.
        let updated = update_job(
            &config,
            &job.id,
            CronJobPatch {
                schedule: Some(Schedule::Cron {
                    expr: "0 0 1 1 *".into(),
                    tz: None,
                }),
                ..CronJobPatch::default()
            },
        )
        .unwrap();

        assert_ne!(
            updated.next_run, original_next,
            "a schedule change must recompute next_run"
        );
        assert_eq!(updated.expression, "0 0 1 1 *");
    }

    #[test]
    fn job_type_from_sql_reads_valid_value() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let now = Utc::now();

        with_connection(&config, |conn| {
            conn.execute(
                "INSERT INTO cron_jobs (id, expression, command, schedule, job_type, created_at, next_run)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    "job-type-valid",
                    "*/5 * * * *",
                    "echo ok",
                    Option::<String>::None,
                    "agent",
                    now.to_rfc3339(),
                    (now + ChronoDuration::minutes(5)).to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .unwrap();

        let job = get_job(&config, "job-type-valid").unwrap();
        assert_eq!(job.job_type, JobType::Agent);
    }

    #[test]
    fn job_type_from_sql_rejects_invalid_value() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let now = Utc::now();

        with_connection(&config, |conn| {
            conn.execute(
                "INSERT INTO cron_jobs (id, expression, command, schedule, job_type, created_at, next_run)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    "job-type-invalid",
                    "*/5 * * * *",
                    "echo ok",
                    Option::<String>::None,
                    "unknown",
                    now.to_rfc3339(),
                    (now + ChronoDuration::minutes(5)).to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .unwrap();

        assert!(get_job(&config, "job-type-invalid").is_err());
    }

    #[test]
    fn migration_falls_back_to_legacy_expression() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        with_connection(&config, |conn| {
            conn.execute(
                "INSERT INTO cron_jobs (id, expression, command, created_at, next_run)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "legacy-id",
                    "*/5 * * * *",
                    "echo legacy",
                    Utc::now().to_rfc3339(),
                    (Utc::now() + ChronoDuration::minutes(5)).to_rfc3339(),
                ],
            )?;
            conn.execute(
                "UPDATE cron_jobs SET schedule = NULL WHERE id = 'legacy-id'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

        let job = get_job(&config, "legacy-id").unwrap();
        assert!(matches!(job.schedule, Schedule::Cron { .. }));
    }

    #[test]
    fn record_and_prune_runs() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp);
        config.cron.max_run_history = 2;
        let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let base = Utc::now();

        for idx in 0..3 {
            let start = base + ChronoDuration::seconds(idx);
            let end = start + ChronoDuration::milliseconds(100);
            record_run(&config, &job.id, start, end, "ok", Some("done"), 100).unwrap();
        }

        let runs = list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn remove_job_cascades_run_history() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let start = Utc::now();
        record_run(
            &config,
            &job.id,
            start,
            start + ChronoDuration::milliseconds(5),
            "ok",
            Some("ok"),
            5,
        )
        .unwrap();

        remove_job(&config, &job.id).unwrap();
        let runs = list_runs(&config, &job.id, 10).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn record_run_truncates_large_output() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo trunc").unwrap();
        let output = "x".repeat(MAX_CRON_OUTPUT_BYTES + 512);

        record_run(
            &config,
            &job.id,
            Utc::now(),
            Utc::now(),
            "ok",
            Some(&output),
            1,
        )
        .unwrap();

        let runs = list_runs(&config, &job.id, 1).unwrap();
        let stored = runs[0].output.as_deref().unwrap_or_default();
        assert!(stored.ends_with(TRUNCATED_OUTPUT_MARKER));
        assert!(stored.len() <= MAX_CRON_OUTPUT_BYTES);
    }

    #[test]
    fn reschedule_after_run_truncates_last_output() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo trunc").unwrap();
        let output = "y".repeat(MAX_CRON_OUTPUT_BYTES + 1024);

        reschedule_after_run(&config, &job, false, &output).unwrap();

        let stored = get_job(&config, &job.id).unwrap();
        let last_output = stored.last_output.as_deref().unwrap_or_default();
        assert!(last_output.ends_with(TRUNCATED_OUTPUT_MARKER));
        assert!(last_output.len() <= MAX_CRON_OUTPUT_BYTES);
    }

    #[test]
    fn record_run_scrubs_credentials_from_output() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo secret").unwrap();
        // Neutral placeholder shaped like a key=value secret the shared scrubber
        // matches (value >= 8 chars). Never a real credential.
        let dummy = "abcd1234efgh5678";
        let output = format!("api_key={dummy}\nall good");

        record_run(
            &config,
            &job.id,
            Utc::now(),
            Utc::now(),
            "ok",
            Some(&output),
            1,
        )
        .unwrap();

        let runs = list_runs(&config, &job.id, 1).unwrap();
        let stored = runs[0].output.as_deref().unwrap_or_default();
        assert!(
            !stored.contains(dummy),
            "raw secret value must not be persisted: {stored}"
        );
        assert!(
            stored.contains("*[REDACTED]"),
            "expected redaction marker in stored output: {stored}"
        );
    }

    #[test]
    fn prepare_cron_output_takes_the_whole_base64_secret() {
        let stored = prepare_cron_output("password=abcdEFGH/ijklMNOP+qrst== done");
        assert_eq!(stored, "password=abcd*[REDACTED] done");
    }

    #[cfg(unix)]
    #[test]
    fn jobs_db_is_restricted_to_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        record_run(
            &config,
            &job.id,
            Utc::now(),
            Utc::now(),
            "ok",
            Some("ok"),
            1,
        )
        .unwrap();

        let db_path = config.workspace_dir.join("cron").join("jobs.db");
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "jobs.db must be owner-only, got {mode:o}");
    }

    #[test]
    fn update_job_refuses_reenabling_a_past_one_shot() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let past = Utc::now() - ChronoDuration::minutes(10);
        let schedule = Schedule::At { at: past };
        let schedule_json = serde_json::to_string(&schedule).unwrap();

        // A fired one-shot that kept its row disabled with a now-past next_run.
        // Written via raw SQL because validate_schedule rejects a past `at`.
        with_connection(&config, |conn| {
            conn.execute(
                "INSERT INTO cron_jobs (id, expression, command, schedule, job_type,
                    session_target, enabled, delivery, delete_after_run, created_at, next_run)
                 VALUES (?1, '', 'echo once', ?2, 'shell', 'isolated', 0, ?3, 0, ?4, ?5)",
                params![
                    "past-oneshot",
                    schedule_json,
                    serde_json::to_string(&DeliveryConfig::default()).unwrap(),
                    past.to_rfc3339(),
                    past.to_rfc3339(),
                ],
            )?;
            Ok(())
        })
        .unwrap();

        // Re-enabling with no new schedule must be refused.
        let err = update_job(
            &config,
            "past-oneshot",
            CronJobPatch {
                enabled: Some(true),
                ..CronJobPatch::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("in the past"),
            "expected a past-one-shot refusal, got: {err}"
        );

        // Escape hatch: a fresh future `at` in the same patch reschedules and succeeds.
        let future = Utc::now() + ChronoDuration::hours(1);
        let ok = update_job(
            &config,
            "past-oneshot",
            CronJobPatch {
                enabled: Some(true),
                schedule: Some(Schedule::At { at: future }),
                ..CronJobPatch::default()
            },
        )
        .unwrap();
        assert!(ok.enabled);
        assert!(ok.next_run > Utc::now());
    }
}
