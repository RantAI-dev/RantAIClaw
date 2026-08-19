use crate::channels::SendMessage;
use crate::config::Config;
use crate::cron::{
    due_jobs, next_run_for_schedule, record_last_run, remove_job, reschedule_after_run, update_job,
    CronJob, CronJobPatch, DeliveryConfig, JobType, Schedule, SessionTarget,
};
use crate::security::SecurityPolicy;
use anyhow::Result;
use chrono::{DateTime, Utc};
use futures_util::{stream, StreamExt};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::time::{self, Duration};

const MIN_POLL_SECONDS: u64 = 5;
const SHELL_JOB_TIMEOUT_SECS: u64 = 120;
const AGENT_JOB_TIMEOUT_SECS: u64 = 600;
const SCHEDULER_COMPONENT: &str = "scheduler";

/// Process-wide set of cron job ids currently executing, shared by BOTH the
/// scheduled poll loop and every manual force-run entry point. A single registry
/// lets a manual "run now" see that a scheduled tick (or a second click) is
/// already running the same job, and refuse to double-execute.
fn in_flight_registry() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static REGISTRY: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// RAII claim on the in-flight registry. Removing the id on `Drop` releases the
/// claim even if the job panics or is cancelled — a post-`await` `remove()` would
/// leak it on any early exit.
struct InFlightGuard {
    id: String,
}

impl InFlightGuard {
    /// Claim `id`. Returns `None` if it is already claimed (still running).
    fn claim(id: &str) -> Option<Self> {
        let mut set = in_flight_registry()
            .lock()
            .expect("in-flight lock poisoned");
        if set.insert(id.to_string()) {
            Some(Self { id: id.to_string() })
        } else {
            None
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = in_flight_registry().lock() {
            set.remove(&self.id);
        }
    }
}

pub async fn run(config: Config) -> Result<()> {
    let poll_secs = config.reliability.scheduler_poll_secs.max(MIN_POLL_SECONDS);
    let mut interval = time::interval(Duration::from_secs(poll_secs));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let security = Arc::new(SecurityPolicy::from_config(
        &config.autonomy,
        &config.workspace_dir,
    ));

    crate::health::mark_component_ok(SCHEDULER_COMPONENT);

    // Due-job batches run on their own tasks so a slow or hung job can never
    // stall interval.tick(). The JoinSet is owned by `run`: when the daemon
    // aborts the scheduler task the set drops and all batch tasks abort with it
    // (a bare tokio::spawn would detach them and leak across shutdown).
    let mut batches: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    // The tick works against `working`, refreshed from disk each cycle so an
    // operator editing scheduler/cron/channel config (or rotating a delivery
    // token) reaches scheduled jobs without a daemon restart — the same reload
    // that already keeps `security` (autonomy) current. Only the poll interval
    // stays fixed at its boot value.
    let mut working = config;

    loop {
        interval.tick().await;
        // Keep scheduler liveness fresh even when there are no due jobs, and even
        // while a previous batch is still running on its own task.
        crate::health::mark_component_ok(SCHEDULER_COMPONENT);

        // Reap finished batch tasks so the set doesn't grow unbounded.
        while let Some(res) = batches.try_join_next() {
            if let Err(e) = res {
                if e.is_panic() {
                    tracing::error!("Scheduler batch task panicked: {e}");
                }
            }
        }

        // Refresh the config half once per poll tick. The scheduler is a
        // long-lived task built at daemon start, so without this an operator
        // tightening autonomy would not reach scheduled jobs until a restart —
        // exactly the surface where nobody is watching. Cost is one config read
        // per interval, floored at MIN_POLL_SECONDS.
        match Config::load_or_init().await {
            Ok(cfg) => {
                security.apply_config(&cfg.autonomy);
                working = cfg;
            }
            // Keep the previous config. Never fall back to a permissive default
            // because a config read failed.
            Err(e) => tracing::warn!(
                target: "scheduler",
                error = %e,
                "config reload failed; keeping the previously applied config for this tick"
            ),
        }

        // rusqlite is blocking file I/O; run the poll query off the async worker
        // so a lock stall can't park a runtime thread shared with gateway/channel
        // work. Clone `working` in — it is reused below to spawn the batch.
        let jobs = {
            let cfg = working.clone();
            match tokio::task::spawn_blocking(move || due_jobs(&cfg, Utc::now())).await {
                Ok(Ok(jobs)) => jobs,
                Ok(Err(e)) => {
                    crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                    tracing::warn!("Scheduler query failed: {e}");
                    continue;
                }
                Err(e) => {
                    crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                    tracing::warn!("Scheduler due_jobs task failed: {e}");
                    continue;
                }
            }
        };

        if jobs.is_empty() {
            continue;
        }

        // Spawn the batch so a slow/hung job can never stall the poll cadence.
        // The process-wide in-flight registry still prevents a job from a
        // still-running earlier batch from being run again. The batch carries the
        // freshly reloaded `working` config, so scheduler/cron/channel edits reach
        // this cycle's jobs.
        let config = working.clone();
        let security = Arc::clone(&security);
        batches.spawn(async move {
            process_due_jobs(&config, &security, jobs, SCHEDULER_COMPONENT).await;
        });
    }
}

pub async fn execute_job_now(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String, Vec<AttemptOutcome>) {
    execute_job_with_retry(config, security, job).await
}

/// Force-run a job now: execute + record run history + update
/// `last_run`/`last_status`/`last_output`. Unlike the scheduled path this does
/// NOT reschedule, auto-delete one-shots, or run delivery — a manual run is for
/// testing and must not shift the schedule or consume a one-shot. Callers must
/// enforce their own security/approval gate before calling.
pub async fn run_job_manual(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String) {
    // Claim the shared in-flight registry so a second "run now" (or an overlapping
    // scheduled tick) can't double-execute the same job. The guard releases the
    // claim on drop, including on panic/cancel. An already-running job returns a
    // clear message and records no run row (it never executed).
    let Some(_guard) = InFlightGuard::claim(&job.id) else {
        return (false, format!("cron job '{}' is already running", job.id));
    };
    let (success, output, attempts) = execute_job_now(config, security, job).await;
    // Record each attempt as its own row (a manual run does not deliver).
    for a in &attempts {
        record_attempt(config, &job.id, a).await;
    }
    let finished_at = attempts.last().map_or_else(Utc::now, |a| a.finished_at);
    if let Err(e) = record_last_run(config, &job.id, finished_at, success, &output) {
        tracing::warn!(job_id = %job.id, error = %e, "failed to record cron last-run fields");
    }
    (success, output)
}

/// One execution attempt of a cron job, with its own timing and outcome. The
/// scheduler records each as a separate run-history row so a retried job shows
/// the real `error, error, ok` sequence and each `duration_ms` reflects only
/// that attempt's execution — not the backoff sleeps between attempts. `pub`
/// because it appears in the return type of the `pub` `execute_job_now`.
pub struct AttemptOutcome {
    attempt: u32,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    success: bool,
    output: String,
}

async fn execute_job_with_retry(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String, Vec<AttemptOutcome>) {
    let mut attempts = Vec::new();
    let mut last_output = String::new();
    let retries = config.reliability.scheduler_retries;
    let mut backoff_ms = config.reliability.provider_backoff_ms.max(200);

    for attempt in 0..=retries {
        let started_at = Utc::now();
        let (success, output) = match job.job_type {
            JobType::Shell => run_job_command(config, security, job).await,
            JobType::Agent => {
                with_timeout(
                    Duration::from_secs(AGENT_JOB_TIMEOUT_SECS),
                    run_agent_job(config, security, job),
                )
                .await
            }
        };
        let finished_at = Utc::now();
        attempts.push(AttemptOutcome {
            attempt: attempt + 1,
            started_at,
            finished_at,
            success,
            output: output.clone(),
        });
        last_output = output;

        if success {
            return (true, last_output, attempts);
        }

        if last_output.starts_with("blocked by security policy:") {
            // Deterministic policy violations are not retryable.
            return (false, last_output, attempts);
        }

        if attempt < retries {
            let jitter_ms = u64::from(Utc::now().timestamp_subsec_millis() % 250);
            time::sleep(Duration::from_millis(backoff_ms + jitter_ms)).await;
            backoff_ms = (backoff_ms.saturating_mul(2)).min(30_000);
        }
    }

    (false, last_output, attempts)
}

async fn process_due_jobs(
    config: &Config,
    security: &Arc<SecurityPolicy>,
    jobs: Vec<CronJob>,
    component: &str,
) {
    // Refresh scheduler health on every successful poll cycle, including idle cycles.
    crate::health::mark_component_ok(component);

    let max_concurrent = config.scheduler.max_concurrent.max(1);
    let mut in_flight_stream = stream::iter(jobs.into_iter().map(|job| {
        let config = config.clone();
        let security = Arc::clone(security);
        let component = component.to_owned();
        async move {
            // Claim the job on the process-wide registry; skip if a previous
            // (long-running) invocation — scheduled or manual — is still going,
            // so a job slower than the poll interval isn't run concurrently. The
            // guard releases the claim on drop, including on panic/cancel.
            let Some(_guard) = InFlightGuard::claim(&job.id) else {
                tracing::warn!(
                    "Scheduler job '{}' still running from a previous tick; skipping this cycle",
                    job.id
                );
                return (job.id.clone(), true);
            };
            execute_and_persist_job(&config, security.as_ref(), &job, &component).await
        }
    }))
    .buffer_unordered(max_concurrent);

    while let Some((job_id, success)) = in_flight_stream.next().await {
        if !success {
            tracing::warn!("Scheduler job '{job_id}' failed");
        }
    }
}

async fn execute_and_persist_job(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    component: &str,
) -> (String, bool) {
    crate::health::mark_component_ok(component);
    warn_if_high_frequency_agent_job(job);

    let (success, output, attempts) = execute_job_with_retry(config, security, job).await;
    let success = persist_job_result(config, job, success, &output, &attempts).await;

    (job.id.clone(), success)
}

async fn run_agent_job(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String) {
    if !security.can_act() {
        return (
            false,
            "blocked by security policy: autonomy is read-only".to_string(),
        );
    }

    if security.is_rate_limited() {
        return (
            false,
            "blocked by security policy: rate limit exceeded".to_string(),
        );
    }

    if !security.record_action() {
        return (
            false,
            "blocked by security policy: action budget exhausted".to_string(),
        );
    }
    let name = job.name.clone().unwrap_or_else(|| "cron-job".to_string());
    let prompt = job.prompt.clone().unwrap_or_default();
    let prefixed_prompt = format!("[cron:{} {name}] {prompt}", job.id);
    let model_override = job.model.clone();

    let run_result = match job.session_target {
        SessionTarget::Main | SessionTarget::Isolated => {
            // Box the agent future: `crate::agent::run_with_scope` is a ~27KB
            // future and is awaited transitively across the whole cron execution
            // chain (execute_job_with_retry → execute_job_now/run_job_manual/
            // execute_and_persist_job). Boxing it once here keeps every enclosing
            // future off the poll-loop stack (clippy::large_futures).
            //
            // Scope memory to `cron:<job_id>` so this job's memory_recall returns
            // its own rows plus the shared/global tier — not another
            // conversation's scoped rows, which it could otherwise quote into the
            // announced output (memory_recall is auto-approved).
            Box::pin(crate::agent::run_with_scope(
                config.clone(),
                Some(prefixed_prompt),
                None,
                model_override,
                config.default_temperature,
                vec![],
                "scheduler",
                Some(format!("cron:{}", job.id)),
            ))
            .await
        }
    };

    match run_result {
        Ok(response) => (
            true,
            if response.trim().is_empty() {
                "agent job executed".to_string()
            } else {
                response
            },
        ),
        Err(e) => (false, format!("agent job failed: {e}")),
    }
}

/// Record one execution attempt as its own run-history row, off the async
/// worker (rusqlite is blocking). A history-write failure is logged, never
/// propagated — the job's outcome must not depend on it; a mid-run deletion of
/// the parent job row is called out distinctly.
async fn record_attempt(config: &Config, job_id: &str, a: &AttemptOutcome) {
    let cfg = config.clone();
    let jid = job_id.to_string();
    let status = if a.success { "ok" } else { "error" };
    let out = a.output.clone();
    let (started, finished, attempt) = (a.started_at, a.finished_at, a.attempt);
    let duration_ms = (finished - started).num_milliseconds();
    match tokio::task::spawn_blocking(move || {
        crate::cron::record_run_attempt(
            &cfg,
            &jid,
            started,
            finished,
            status,
            Some(&out),
            duration_ms,
            attempt,
        )
    })
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            if crate::cron::get_job(config, job_id).is_err() {
                tracing::warn!(job_id = %job_id, "cron job deleted while running; run history not recorded");
            } else {
                tracing::warn!(job_id = %job_id, error = %e, "failed to record cron run history");
            }
        }
        Err(e) => {
            tracing::warn!(job_id = %job_id, error = %e, "failed to join cron record_run task");
        }
    }
}

async fn persist_job_result(
    config: &Config,
    job: &CronJob,
    success: bool,
    output: &str,
    attempts: &[AttemptOutcome],
) -> bool {
    // The final attempt's finish time stamps last_run / reschedule below.
    let finished_at = attempts.last().map_or_else(Utc::now, |a| a.finished_at);

    // Record each EXECUTION attempt as its own run-history row — a retried job
    // now shows the real error,error,ok sequence and each duration_ms excludes
    // the backoff sleeps between attempts. `last_status` / the rows describe
    // whether the JOB ran — delivery is a separate concern and must never flip
    // this (a chat hiccup is not a job failure).
    for a in attempts {
        record_attempt(config, &job.id, a).await;
    }

    // Deliver only a job that actually succeeded and whose output is not a
    // security refusal. A refused job's output is the rejected command text +
    // policy internals; announcing it verbatim would leak it into the configured
    // chat. Delivery is best-effort: its failure is logged, never recorded as a
    // job error.
    if success && !is_security_refusal(output) {
        if let Err(e) = deliver_if_configured(config, job, output).await {
            if job.delivery.best_effort {
                tracing::warn!("Cron delivery failed (best_effort): {e}");
            } else {
                tracing::warn!("Cron delivery failed: {e}");
            }
        }
    } else if job.delivery.mode.eq_ignore_ascii_case("announce") {
        // Announce was requested but withheld: never push failed/refused output
        // into chat. Stated once, without the raw output.
        tracing::warn!(
            "Cron job '{}' output withheld from delivery ({})",
            job.id,
            if is_security_refusal(output) {
                "security refusal"
            } else {
                "job failed"
            }
        );
    }

    if is_one_shot(job) {
        if job.delete_after_run && success {
            let cfg = config.clone();
            let job_id = job.id.clone();
            match tokio::task::spawn_blocking(move || remove_job(&cfg, &job_id)).await {
                Ok(Err(e)) => {
                    tracing::warn!("Failed to remove one-shot cron job after success: {e}");
                }
                Err(e) => tracing::warn!("Failed to join cron remove_job task: {e}"),
                Ok(Ok(())) => {}
            }
        } else {
            // Not opted into auto-delete (or it failed): keep the row for history
            // but disable it so the poller can't re-fire this already-past `At`.
            if let Err(e) = record_last_run(config, &job.id, finished_at, success, output) {
                tracing::warn!(job_id = %job.id, error = %e, "failed to record cron last-run fields");
            }
            let cfg = config.clone();
            let job_id = job.id.clone();
            match tokio::task::spawn_blocking(move || {
                update_job(
                    &cfg,
                    &job_id,
                    CronJobPatch {
                        enabled: Some(false),
                        ..CronJobPatch::default()
                    },
                )
            })
            .await
            {
                Ok(Err(e)) => tracing::warn!("Failed to disable one-shot cron job: {e}"),
                Err(e) => tracing::warn!("Failed to join cron update_job task: {e}"),
                Ok(Ok(_)) => {}
            }
        }
        return success;
    }

    {
        let cfg = config.clone();
        let job_clone = job.clone();
        let out = output.to_string();
        match tokio::task::spawn_blocking(move || {
            reschedule_after_run(&cfg, &job_clone, success, &out)
        })
        .await
        {
            Ok(Err(e)) => tracing::warn!("Failed to persist scheduler run result: {e}"),
            Err(e) => tracing::warn!("Failed to join cron reschedule task: {e}"),
            Ok(Ok(())) => {}
        }
    }

    success
}

/// An `At` job fires exactly once — there is no "next" occurrence, so it must
/// never be rescheduled (its next_run would be the same, now-past, instant,
/// which the poller would re-select every cycle → infinite re-fire). After it
/// runs we either delete it (`delete_after_run` opt-in, on success) or disable
/// it (keeping the row for its run history).
fn is_one_shot(job: &CronJob) -> bool {
    matches!(job.schedule, Schedule::At { .. })
}

/// True when an *agent* job is scheduled more often than every 5 minutes. For
/// `Cron`, compares the gap between two CONSECUTIVE occurrences (`next(a)` after
/// `a`) — NOT `next(now)` vs `next(now + 1s)`, which return the same occurrence
/// unless one happens to fall in that 1-second window. That old comparison gave
/// a ~0 gap and warned on every cron agent job, including a daily `0 9 * * *`.
fn is_high_frequency_agent_job(job: &CronJob) -> bool {
    if !matches!(job.job_type, JobType::Agent) {
        return false;
    }
    match &job.schedule {
        Schedule::Every { every_ms } => *every_ms < 5 * 60 * 1000,
        Schedule::Cron { .. } => {
            let now = Utc::now();
            match next_run_for_schedule(&job.schedule, now) {
                Ok(a) => match next_run_for_schedule(&job.schedule, a) {
                    Ok(b) => (b - a).num_minutes() < 5,
                    Err(_) => false,
                },
                Err(_) => false,
            }
        }
        Schedule::At { .. } => false,
    }
}

fn warn_if_high_frequency_agent_job(job: &CronJob) {
    if is_high_frequency_agent_job(job) {
        tracing::warn!(
            "Cron agent job '{}' is scheduled more frequently than every 5 minutes",
            job.id
        );
    }
}

/// The scheduled path's security refusals (autonomy read-only, rate limit,
/// command not allowed, risk gate, forbidden path, budget) all begin with this
/// exact prefix (see `run_job_command_with_timeout`). Delivery must never push
/// such a string into a chat: it carries the rejected command text and policy
/// internals.
const SECURITY_REFUSAL_PREFIX: &str = "blocked by security policy:";

fn is_security_refusal(output: &str) -> bool {
    output.starts_with(SECURITY_REFUSAL_PREFIX)
}

async fn deliver_if_configured(config: &Config, job: &CronJob, output: &str) -> Result<()> {
    let delivery: &DeliveryConfig = &job.delivery;
    if !delivery.mode.eq_ignore_ascii_case("announce") {
        return Ok(());
    }

    let channel = delivery
        .channel
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("delivery.channel is required for announce mode"))?;
    let target = delivery
        .to
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("delivery.to is required for announce mode"))?;
    // Fail-safe for jobs written before the creation-time gate (plan 173 Step 1)
    // or via a path it does not cover: never announce to an empty target.
    if target.trim().is_empty() {
        anyhow::bail!("delivery.to is empty; refusing to announce to an unspecified target");
    }

    // Construction goes through the channels factory, so cron cannot drift from
    // what the runtime and the doctor build — these four were hand-rolled here
    // with their own copies of every constructor argument list.
    //
    // The *gate* deliberately stays `channel_supports_announce_delivery`. The
    // factory can build fifteen channels; widening delivery to all of them is a
    // capability change, not a refactor, so it is surfaced rather than taken.
    let key = channel.to_ascii_lowercase();
    if !crate::channels::channel_supports_announce_delivery(&key) {
        anyhow::bail!("unsupported delivery channel: {key}");
    }

    // Build only the one channel this delivery needs — not the whole fleet of
    // ~15 — and emit no construction-time warnings on this per-run path (the
    // Slack app_token note lives on the startup/doctor paths instead).
    let Some(channel_impl) = crate::channels::build_one(config, &key) else {
        anyhow::bail!("{key} channel not configured");
    };
    channel_impl.send(&SendMessage::new(output, target)).await?;

    Ok(())
}

fn is_env_assignment(word: &str) -> bool {
    word.contains('=')
        && word
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

fn strip_wrapping_quotes(token: &str) -> &str {
    token.trim_matches(|c| c == '"' || c == '\'')
}

fn forbidden_path_argument(security: &SecurityPolicy, command: &str) -> Option<String> {
    let mut normalized = command.to_string();
    for sep in ["&&", "||"] {
        normalized = normalized.replace(sep, "\x00");
    }
    for sep in ['\n', ';', '|'] {
        normalized = normalized.replace(sep, "\x00");
    }

    for segment in normalized.split('\x00') {
        let tokens: Vec<&str> = segment.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }

        // Skip leading env assignments and executable token.
        let mut idx = 0;
        while idx < tokens.len() && is_env_assignment(tokens[idx]) {
            idx += 1;
        }
        if idx >= tokens.len() {
            continue;
        }
        idx += 1;

        for token in &tokens[idx..] {
            let candidate = strip_wrapping_quotes(token);
            if candidate.is_empty() || candidate.contains("://") {
                continue;
            }

            if candidate.starts_with('-') {
                // A forbidden path can hide in a flag VALUE — `--file=/etc/shadow`
                // or glued to a short flag as `-o/etc/passwd` — which the old
                // blanket skip of any `-`-token let straight through. Inspect the
                // value portion so it faces the same path check as a bare token.
                if let Some((_, value)) = candidate.split_once('=') {
                    let value = strip_wrapping_quotes(value);
                    if path_is_forbidden(security, value) {
                        return Some(value.to_string());
                    }
                } else if let Some(stripped) = candidate.strip_prefix('-') {
                    // Glued short flag (single leading dash, no `=`): the value is
                    // the remainder after the flag letter. A long flag (`--x`) has
                    // no glued value, so skip it.
                    if !stripped.starts_with('-') {
                        if let Some(rest) = stripped.get(1..) {
                            let rest = strip_wrapping_quotes(rest);
                            if path_is_forbidden(security, rest) {
                                return Some(rest.to_string());
                            }
                        }
                    }
                }
                continue;
            }

            if path_is_forbidden(security, candidate) {
                return Some(candidate.to_string());
            }
        }
    }

    None
}

/// Whether `s` looks like a path and is not admitted by the security policy.
/// Shared by the bare-token and flag-value branches of the forbidden-path guard
/// so the two cannot drift apart.
fn path_is_forbidden(security: &SecurityPolicy, s: &str) -> bool {
    let looks_like_path = s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with("~/")
        || s.contains('/');
    looks_like_path && !security.is_path_allowed(s)
}

/// Apply a wall-clock timeout to a job future, returning a uniform timed-out
/// result. Used to bound agent jobs (which call `crate::agent::run` and have no
/// inner timeout of their own, unlike shell jobs).
async fn with_timeout(
    timeout: Duration,
    fut: impl std::future::Future<Output = (bool, String)>,
) -> (bool, String) {
    match time::timeout(timeout, fut).await {
        Ok(result) => result,
        Err(_) => (
            false,
            format!("agent job timed out after {}s", timeout.as_secs_f64()),
        ),
    }
}

async fn run_job_command(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
) -> (bool, String) {
    run_job_command_with_timeout(
        config,
        security,
        job,
        Duration::from_secs(SHELL_JOB_TIMEOUT_SECS),
    )
    .await
}

/// The first whitespace token of a shell command, reduced to its basename.
/// Refusal messages are stored in run history and served over the API, so they
/// identify the rejected *program* for debugging without echoing the full
/// command line (whose arguments can carry secrets).
fn program_basename(command: &str) -> &str {
    command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
}

async fn run_job_command_with_timeout(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    timeout: Duration,
) -> (bool, String) {
    if !security.can_act() {
        return (
            false,
            "blocked by security policy: autonomy is read-only".to_string(),
        );
    }

    if security.is_rate_limited() {
        return (
            false,
            "blocked by security policy: rate limit exceeded".to_string(),
        );
    }

    if !security.is_command_allowed(&job.command) {
        return (
            false,
            format!(
                "blocked by security policy: command not allowed (job {}, program `{}`)",
                job.id,
                program_basename(&job.command),
            ),
        );
    }

    // Risk classification. Deliberately after `is_command_allowed` so the
    // allowlist keeps its own (lowercase) message, and this check only ever
    // fires for the risk gate. `approved` is `false`: on the scheduled path
    // there is by definition no operator present to approve anything.
    if let Err(reason) = security.validate_command_execution(&job.command, false) {
        return (false, format!("blocked by security policy: {reason}"));
    }

    if let Some(path) = forbidden_path_argument(security, &job.command) {
        return (
            false,
            format!("blocked by security policy: forbidden path argument: {path}"),
        );
    }

    if !security.record_action() {
        return (
            false,
            "blocked by security policy: action budget exhausted".to_string(),
        );
    }

    let child = match Command::new("sh")
        .arg("-lc")
        .arg(&job.command)
        .current_dir(&config.workspace_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return (false, format!("spawn error: {e}")),
    };

    match time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = format!(
                "status={}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                stdout.trim(),
                stderr.trim()
            );
            (output.status.success(), combined)
        }
        Ok(Err(e)) => (false, format!("spawn error: {e}")),
        Err(_) => (
            false,
            format!("job timed out after {}s", timeout.as_secs_f64()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::cron::{self, DeliveryConfig};
    use crate::security::SecurityPolicy;
    use chrono::{Duration as ChronoDuration, Utc};
    use tempfile::TempDir;

    async fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        tokio::fs::create_dir_all(&config.workspace_dir)
            .await
            .unwrap();
        config
    }

    fn test_job(command: &str) -> CronJob {
        CronJob {
            id: "test-job".into(),
            expression: "* * * * *".into(),
            schedule: crate::cron::Schedule::Cron {
                expr: "* * * * *".into(),
                tz: None,
            },
            command: command.into(),
            prompt: None,
            name: None,
            job_type: JobType::Shell,
            session_target: SessionTarget::Isolated,
            model: None,
            enabled: true,
            delivery: DeliveryConfig::default(),
            delete_after_run: false,
            created_at: Utc::now(),
            next_run: Utc::now(),
            last_run: None,
            last_status: None,
            last_output: None,
            created_by: None,
        }
    }

    fn unique_component(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    /// A single-attempt outcome slice for driving `persist_job_result` in tests
    /// that don't exercise retries.
    fn one_attempt(
        started: DateTime<Utc>,
        finished: DateTime<Utc>,
        success: bool,
        output: &str,
    ) -> Vec<AttemptOutcome> {
        vec![AttemptOutcome {
            attempt: 1,
            started_at: started,
            finished_at: finished,
            success,
            output: output.to_string(),
        }]
    }

    #[tokio::test]
    async fn run_job_command_success() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("echo scheduler-ok");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(success);
        assert!(output.contains("scheduler-ok"));
        assert!(output.contains("status=exit status: 0"));
    }

    #[tokio::test]
    async fn run_job_command_failure() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("ls definitely_missing_file_for_scheduler_test");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("definitely_missing_file_for_scheduler_test"));
        assert!(output.contains("status=exit status:"));
    }

    #[tokio::test]
    async fn run_job_command_times_out() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.allowed_commands = vec!["sleep".into()];
        let job = test_job("sleep 1");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) =
            run_job_command_with_timeout(&config, &security, &job, Duration::from_millis(50)).await;
        assert!(!success);
        assert!(output.contains("job timed out after"));
    }

    /// The scheduled path never called the risk gate, so a cron job could run
    /// a command the same policy would refuse from an interactive turn. There
    /// is no operator present to approve at fire time, so it is refused.
    ///
    /// `chmod` is on the allowlist here on purpose: without that the allowlist
    /// refuses first and the test would pass on pre-fix code.
    #[tokio::test]
    async fn scheduled_run_refuses_a_high_risk_command_under_supervised() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.allowed_commands = vec!["echo".into(), "chmod".into()];
        config.autonomy.block_high_risk_commands = false;
        let job = test_job("chmod 644 f");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (_success, output) = run_job_command(&config, &security, &job).await;
        assert!(
            output.contains("blocked by security policy"),
            "high-risk scheduled command should be refused by the risk gate, got: {output}"
        );
    }

    /// The scheduler builds its policy once at daemon start and holds it for
    /// the process lifetime, so before the per-tick refresh an operator
    /// tightening autonomy would not reach scheduled jobs until a restart —
    /// the one surface where nobody is watching.
    #[tokio::test]
    async fn cron_scheduler_applies_an_autonomy_change() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.allowed_commands = vec!["echo".into()];
        let job = test_job("echo still-running");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, _) = run_job_command(&config, &security, &job).await;
        assert!(success, "baseline: the job runs under Supervised");

        security.apply_config(&crate::config::AutonomyConfig {
            level: crate::security::AutonomyLevel::ReadOnly,
            ..config.autonomy.clone()
        });

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(
            !success,
            "a tightened autonomy must reach the scheduled path"
        );
        assert!(
            output.contains("read-only"),
            "refusal should name the autonomy level, got: {output}"
        );
    }

    /// Guard against over-blocking: the risk gate must not refuse everything.
    #[tokio::test]
    async fn scheduled_run_still_allows_a_low_risk_allowlisted_command() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.allowed_commands = vec!["echo".into()];
        let job = test_job("echo scheduled-ok");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(
            success,
            "low-risk allowlisted command must still run: {output}"
        );
        assert!(output.contains("scheduled-ok"));
    }

    #[tokio::test]
    async fn run_job_command_blocks_disallowed_command() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.allowed_commands = vec!["echo".into()];
        let job = test_job("curl https://evil.example");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("command not allowed"));
        // The rejected program's basename is identifiable for debugging...
        assert!(
            output.contains("curl"),
            "program name should survive: {output}"
        );
        // ...but the argument (a URL here, which could carry a token) is not
        // echoed into stored, API-served run history.
        assert!(
            !output.contains("evil.example"),
            "command arguments must not be echoed: {output}"
        );
    }

    #[tokio::test]
    async fn run_job_command_blocks_forbidden_path_argument() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.allowed_commands = vec!["cat".into()];
        let job = test_job("cat /etc/passwd");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("forbidden path argument"));
        assert!(output.contains("/etc/passwd"));
    }

    #[tokio::test]
    async fn run_job_command_blocks_forbidden_path_in_long_flag_value() {
        // A forbidden path hidden in a `--flag=value` must be caught, not skipped
        // as an ordinary flag.
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.allowed_commands = vec!["cat".into()];
        let job = test_job("cat --file=/etc/shadow");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("forbidden path argument"), "{output}");
        assert!(output.contains("/etc/shadow"), "{output}");
    }

    #[tokio::test]
    async fn run_job_command_blocks_forbidden_path_in_glued_short_flag() {
        // `-o/etc/passwd` glues the path to a short flag.
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.allowed_commands = vec!["cat".into()];
        let job = test_job("cat -o/etc/passwd");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("forbidden path argument"), "{output}");
        assert!(output.contains("/etc/passwd"), "{output}");
    }

    #[tokio::test]
    async fn run_job_command_allows_workspace_path_in_flag_value() {
        // Negative control: a workspace-relative path in a flag value must NOT be
        // refused as a forbidden path (guards against over-blocking). The `./`
        // prefix makes it path-shaped so the value actually reaches is_path_allowed.
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.allowed_commands = vec!["cat".into()];
        let job = test_job("cat --file=./notes.txt");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (_success, output) = run_job_command(&config, &security, &job).await;
        assert!(
            !output.contains("forbidden path argument"),
            "a workspace path must not be blocked: {output}"
        );
    }

    #[tokio::test]
    async fn run_job_command_blocks_readonly_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        let job = test_job("echo should-not-run");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("read-only"));
    }

    #[tokio::test]
    async fn run_job_command_blocks_rate_limited() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.max_actions_per_hour = 0;
        let job = test_job("echo should-not-run");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("rate limit exceeded"));
    }

    #[tokio::test]
    async fn execute_job_with_retry_recovers_after_first_failure() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.reliability.scheduler_retries = 1;
        config.reliability.provider_backoff_ms = 1;
        config.autonomy.allowed_commands = vec!["sh".into()];
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        tokio::fs::write(
            config.workspace_dir.join("retry-once.sh"),
            "#!/bin/sh\nif [ -f retry-ok.flag ]; then\n  echo recovered\n  exit 0\nfi\ntouch retry-ok.flag\nexit 1\n",
        )
        .await
        .unwrap();
        let job = test_job("sh ./retry-once.sh");

        let (success, output, _attempts) = execute_job_with_retry(&config, &security, &job).await;
        assert!(success);
        assert!(output.contains("recovered"));
    }

    #[tokio::test]
    async fn execute_job_with_retry_exhausts_attempts() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.reliability.scheduler_retries = 1;
        config.reliability.provider_backoff_ms = 1;
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let job = test_job("ls always_missing_for_retry_test");

        let (success, output, _attempts) = execute_job_with_retry(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("always_missing_for_retry_test"));
    }

    #[tokio::test]
    async fn retried_job_records_one_row_per_attempt() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.cron.max_run_history = 20; // keep both attempt rows
        let job = cron::add_job(&config, "*/5 * * * *", "echo hi").unwrap();

        // A fail-then-succeed execution: two attempts, ~300ms apart (the backoff),
        // each running ~5ms. The rows must show the real sequence and per-attempt
        // durations, not one clean row with the whole-window duration.
        let t0 = Utc::now();
        let attempts = vec![
            AttemptOutcome {
                attempt: 1,
                started_at: t0,
                finished_at: t0 + ChronoDuration::milliseconds(5),
                success: false,
                output: "boom".into(),
            },
            AttemptOutcome {
                attempt: 2,
                started_at: t0 + ChronoDuration::milliseconds(300),
                finished_at: t0 + ChronoDuration::milliseconds(305),
                success: true,
                output: "ok".into(),
            },
        ];
        let success = persist_job_result(&config, &job, true, "ok", &attempts).await;
        assert!(success);

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 2, "each attempt is its own run-history row");
        // Newest-first (started_at DESC): attempt 2 (ok) then attempt 1 (error).
        assert_eq!(runs[0].attempt, 2);
        assert_eq!(runs[0].status, "ok");
        assert_eq!(runs[1].attempt, 1);
        assert_eq!(runs[1].status, "error");
        // Each duration reflects only that attempt (~5ms), not the ~300ms gap.
        assert!(runs[0].duration_ms.unwrap() < 100);
        assert!(runs[1].duration_ms.unwrap() < 100);
    }

    #[tokio::test]
    async fn single_attempt_success_records_one_row_with_attempt_one() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "*/5 * * * *", "echo hi").unwrap();

        let t0 = Utc::now();
        let attempts = one_attempt(t0, t0 + ChronoDuration::milliseconds(5), true, "ok");
        let success = persist_job_result(&config, &job, true, "ok", &attempts).await;
        assert!(success);

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].attempt, 1);
        assert_eq!(runs[0].status, "ok");
    }

    fn agent_job_with_cron(expr: &str) -> CronJob {
        let mut job = test_job("noop");
        job.job_type = JobType::Agent;
        job.prompt = Some("hi".into());
        job.schedule = crate::cron::Schedule::Cron {
            expr: expr.into(),
            tz: None,
        };
        job
    }

    #[test]
    fn high_frequency_predicate_ignores_daily_agent_job() {
        // The regression: `next(now)` vs `next(now+1s)` returned the same
        // occurrence and warned on every cron agent job. A daily job must not.
        assert!(!is_high_frequency_agent_job(&agent_job_with_cron(
            "0 9 * * *"
        )));
    }

    #[test]
    fn high_frequency_predicate_flags_every_minute_agent_job() {
        assert!(is_high_frequency_agent_job(&agent_job_with_cron(
            "*/1 * * * *"
        )));
    }

    #[test]
    fn high_frequency_predicate_ignores_shell_job() {
        // A shell job is never an agent job, so it is never warned about,
        // regardless of frequency.
        let mut job = test_job("echo hi");
        job.schedule = crate::cron::Schedule::Cron {
            expr: "*/1 * * * *".into(),
            tz: None,
        };
        assert!(!is_high_frequency_agent_job(&job));
    }

    #[tokio::test]
    async fn run_agent_job_returns_error_without_provider_key() {
        // This is the only agent-job test that clears the security gates and
        // reaches the real `agent::run`, which records the turn through
        // `open_cli_session_store`. That resolves the ACTIVE PROFILE from
        // `HOME`, not from the `Config` handed in below — so the TempDir config
        // isolates the cron DB but not session history, and every run appended a
        // `[cron:test-job cron-job] Say hello` row to the operator's real
        // sessions.db. Pin `HOME` under the crate-wide lock.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let home = TempDir::new().unwrap();
        let _restore = crate::test_env::HomeGuard::set(home.path());

        // Prove the pin took rather than trusting it.
        let db = crate::profile::ProfileManager::active()
            .expect("active profile")
            .sessions_db_path();
        assert!(
            db.starts_with(home.path()),
            "test must own its sessions.db; resolved {db:?} outside {:?}",
            home.path()
        );

        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_agent_job(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("agent job failed:"));
    }

    #[tokio::test]
    async fn run_agent_job_blocks_readonly_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_agent_job(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("read-only"));
    }

    #[tokio::test]
    async fn run_agent_job_blocks_rate_limited() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.max_actions_per_hour = 0;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_agent_job(&config, &security, &job).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("rate limit exceeded"));
    }

    #[tokio::test]
    async fn process_due_jobs_marks_component_ok_even_when_idle() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let security = Arc::new(SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));
        let component = unique_component("scheduler-idle");

        crate::health::mark_component_error(&component, "pre-existing error");
        process_due_jobs(&config, &security, Vec::new(), &component).await;

        let snapshot = crate::health::snapshot_json();
        let entry = &snapshot["components"][component.as_str()];
        assert_eq!(entry["status"], "ok");
        assert!(entry["last_ok"].as_str().is_some());
        assert!(entry["last_error"].is_null());
    }

    #[tokio::test]
    async fn process_due_jobs_failure_does_not_mark_component_unhealthy() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("ls definitely_missing_file_for_scheduler_component_health_test");
        let security = Arc::new(SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));
        let component = unique_component("scheduler-fail");

        crate::health::mark_component_ok(&component);
        process_due_jobs(&config, &security, vec![job], &component).await;

        let snapshot = crate::health::snapshot_json();
        let entry = &snapshot["components"][component.as_str()];
        assert_eq!(entry["status"], "ok");
    }

    #[tokio::test]
    async fn process_due_jobs_skips_job_already_in_flight() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "*/5 * * * *", "echo hi").unwrap();
        let security = Arc::new(SecurityPolicy::from_config(
            &config.autonomy,
            &config.workspace_dir,
        ));
        // Pretend the job is still running from a previous tick by holding the
        // process-wide in-flight claim across the poll. The job id is a unique
        // UUID (cron::add_job), so this global claim cannot bleed into other tests.
        let _held = InFlightGuard::claim(&job.id).expect("fresh id must claim");
        let component = unique_component("scheduler-inflight");

        process_due_jobs(&config, &security, vec![job.clone()], &component).await;

        // It must have been skipped → no run recorded.
        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert!(
            runs.is_empty(),
            "an in-flight job must be skipped, not executed"
        );
    }

    #[tokio::test]
    async fn run_job_manual_refuses_a_concurrent_run_of_the_same_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "*/5 * * * *", "echo hi").unwrap();
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        // While the job is claimed (simulating an in-flight run), a manual run
        // must refuse and record NO run row.
        {
            let _held = InFlightGuard::claim(&job.id).expect("fresh id must claim");
            let (success, output) = run_job_manual(&config, &security, &job).await;
            assert!(!success, "a concurrent manual run must not execute");
            assert!(
                output.contains("already running"),
                "expected an 'already running' message, got: {output}"
            );
            assert!(
                cron::list_runs(&config, &job.id, 10).unwrap().is_empty(),
                "the refused run must not record a run row"
            );
        }
        // Claim released → a manual run now executes and records exactly one row.
        let (success, _) = run_job_manual(&config, &security, &job).await;
        assert!(success);
        assert_eq!(cron::list_runs(&config, &job.id, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn persist_job_result_records_run_and_reschedules_shell_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(
            &config,
            &job,
            true,
            "ok",
            &one_attempt(started, finished, true, "ok"),
        )
        .await;
        assert!(success);

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        let updated = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn persist_job_result_delivery_failure_does_not_mark_job_errored() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        // Announce to a telegram channel NOT configured in the test Config, so
        // deliver_if_configured returns Err. With best_effort=false this used to
        // flip the recorded status to "error" for a job that executed fine.
        let mut job = cron::add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("telegram".into()),
            to: Some("123".into()),
            best_effort: false,
        };
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(
            &config,
            &job,
            true,
            "job ran fine",
            &one_attempt(started, finished, true, "job ran fine"),
        )
        .await;
        assert!(success, "a delivery failure must not fail the job");
        assert_eq!(
            cron::get_job(&config, &job.id)
                .unwrap()
                .last_status
                .as_deref(),
            Some("ok"),
            "recorded status must reflect execution, not delivery"
        );
    }

    #[tokio::test]
    async fn persist_job_result_does_not_deliver_a_security_refusal() {
        // The marker is this test's teeth. (The full "delivery is not invoked on
        // a refusal" behaviour isn't isolatable in this unit harness without a
        // delivery spy; the marker plus the success/refusal gate in
        // persist_job_result are what suppress it.)
        assert!(is_security_refusal(
            "blocked by security policy: command not allowed: example"
        ));
        assert!(!is_security_refusal("all good"));

        // Documentation (holds before and after the fix): a refused job records
        // "error" and returns false.
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);
        let success = persist_job_result(
            &config,
            &job,
            false,
            "blocked by security policy: command not allowed: example",
            &one_attempt(
                started,
                finished,
                false,
                "blocked by security policy: command not allowed: example",
            ),
        )
        .await;
        assert!(!success);
        assert_eq!(
            cron::get_job(&config, &job.id)
                .unwrap()
                .last_status
                .as_deref(),
            Some("error")
        );
    }

    #[tokio::test]
    async fn persist_job_result_success_deletes_one_shot() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            Some("one-shot".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            true,
            None,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(
            &config,
            &job,
            true,
            "ok",
            &one_attempt(started, finished, true, "ok"),
        )
        .await;
        assert!(success);
        let lookup = cron::get_job(&config, &job.id);
        assert!(lookup.is_err());
    }

    #[tokio::test]
    async fn persist_job_result_failure_disables_one_shot() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            Some("one-shot".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            true,
            None,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(
            &config,
            &job,
            false,
            "boom",
            &one_attempt(started, finished, false, "boom"),
        )
        .await;
        assert!(!success);
        let updated = cron::get_job(&config, &job.id).unwrap();
        assert!(!updated.enabled);
        assert_eq!(updated.last_status.as_deref(), Some("error"));
    }

    #[tokio::test]
    async fn run_job_manual_records_without_rescheduling() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let before = cron::get_job(&config, &job.id).unwrap().next_run;

        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let (ok, _) = run_job_manual(&config, &security, &job).await;
        assert!(ok);

        let after = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(
            after.next_run, before,
            "a manual run must NOT reschedule the job"
        );
        assert_eq!(cron::list_runs(&config, &job.id, 10).unwrap().len(), 1);
        assert_eq!(after.last_status.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn run_job_manual_survives_missing_job_row() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        // A job value whose row was never inserted: recording its run fails the FK
        // INSERT internally, but that must not fail the run or panic. (Logging is a
        // side-effect this test does not assert; the branch choice is covered by
        // the grep-based done criteria in the plan.) A unique id avoids colliding
        // with another test's process-wide in-flight claim on the shared "test-job".
        let mut job = test_job("echo ok");
        job.id = "missing-row-probe".into();

        let (ok, output) = run_job_manual(&config, &security, &job).await;
        assert!(ok, "the command ran successfully");
        assert!(output.contains("ok"));
        // No run row exists because the parent job row is absent — the write
        // failure was logged and swallowed here, not propagated.
        assert!(cron::list_runs(&config, &job.id, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn with_timeout_reports_timeout_for_slow_job() {
        let (ok, msg) = with_timeout(Duration::from_millis(20), async {
            time::sleep(Duration::from_secs(30)).await;
            (true, "should not finish".to_string())
        })
        .await;
        assert!(!ok);
        assert!(msg.contains("timed out"), "{msg}");
    }

    #[tokio::test]
    async fn with_timeout_passes_through_fast_job() {
        let (ok, msg) = with_timeout(Duration::from_secs(5), async {
            (true, "quick".to_string())
        })
        .await;
        assert!(ok);
        assert_eq!(msg, "quick");
    }

    #[tokio::test]
    async fn persist_job_result_disables_shell_one_shot_instead_of_refiring() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        // Shell one-shot as created by CLI `add-at`/`once`: delete_after_run = false.
        let job = cron::add_shell_job(
            &config,
            Some("one-shot-shell".into()),
            crate::cron::Schedule::At { at },
            "echo hi",
            None,
            false,
            None,
        )
        .unwrap();
        assert!(
            !job.delete_after_run,
            "shell one-shot has delete_after_run=false"
        );
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(
            &config,
            &job,
            true,
            "ok",
            &one_attempt(started, finished, true, "ok"),
        )
        .await;
        assert!(success);

        // Must survive (user did NOT opt into auto-delete) …
        let stored = cron::get_job(&config, &job.id).unwrap();
        // … but be DISABLED so it never re-fires. Regression: it used to reschedule
        // next_run to the past `at` instant and re-run on every poll cycle forever.
        assert!(
            !stored.enabled,
            "a fired one-shot At job must be disabled, not rescheduled"
        );
        assert_eq!(stored.last_status.as_deref(), Some("ok"));
        // And it must not be selected as due again.
        let due = cron::due_jobs(&config, Utc::now() + ChronoDuration::days(365)).unwrap();
        assert!(
            due.iter().all(|j| j.id != job.id),
            "disabled one-shot must not be due"
        );
    }

    #[tokio::test]
    async fn persist_job_result_deletes_shell_one_shot_when_flagged() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_shell_job(
            &config,
            Some("one-shot-shell".into()),
            crate::cron::Schedule::At { at },
            "echo hi",
            None,
            true, // delete_after_run — now honored for shell jobs
            None,
        )
        .unwrap();
        assert!(
            job.delete_after_run,
            "shell one-shot must carry the flag now"
        );

        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);
        let success = persist_job_result(
            &config,
            &job,
            true,
            "ok",
            &one_attempt(started, finished, true, "ok"),
        )
        .await;
        assert!(success);

        // It opted into auto-delete → the row must be gone.
        assert!(
            cron::get_job(&config, &job.id).is_err(),
            "a flagged shell one-shot must self-delete after a successful run"
        );
    }

    #[tokio::test]
    async fn persist_job_result_keeps_run_history_for_undeleted_one_shot() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        // Agent one-shot with delete_after_run = false (the no-delivery default
        // after the fix): must be kept+disabled, and its run row must survive
        // the cron_runs FK cascade.
        let job = cron::add_agent_job(
            &config,
            Some("one-shot".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            crate::cron::SessionTarget::Isolated,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);

        let success = persist_job_result(
            &config,
            &job,
            true,
            "ok",
            &one_attempt(started, finished, true, "ok"),
        )
        .await;
        assert!(success);

        let stored = cron::get_job(&config, &job.id).unwrap();
        assert!(!stored.enabled, "kept one-shot must be disabled");
        assert_eq!(
            cron::list_runs(&config, &job.id, 10).unwrap().len(),
            1,
            "the run-history row must survive (no cascade delete)"
        );
    }

    #[tokio::test]
    async fn deliver_if_configured_handles_none_and_invalid_channel() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let mut job = test_job("echo ok");

        assert!(deliver_if_configured(&config, &job, "x").await.is_ok());

        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("invalid".into()),
            to: Some("target".into()),
            best_effort: true,
        };
        let err = deliver_if_configured(&config, &job, "x").await.unwrap_err();
        assert!(err.to_string().contains("unsupported delivery channel"));
    }

    #[tokio::test]
    async fn deliver_if_configured_rejects_empty_target() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let mut job = test_job("echo ok");
        // Announce on a supported channel but with a whitespace `to`: must error
        // (fail-safe), never announce to an unspecified target.
        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("telegram".into()),
            to: Some("   ".into()),
            best_effort: true,
        };
        let err = deliver_if_configured(&config, &job, "x").await.unwrap_err();
        assert!(err.to_string().contains("empty"), "got: {err}");
    }
}
