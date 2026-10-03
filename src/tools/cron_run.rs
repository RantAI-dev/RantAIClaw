use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron::{self, JobType};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct CronRunTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}

impl CronRunTool {
    pub fn new(config: Arc<Config>, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }
}

#[async_trait]
impl Tool for CronRunTool {
    fn name(&self) -> &str {
        "cron_run"
    }

    fn description(&self) -> &str {
        "Force-run a cron job immediately and record run history"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string" },
                "origin_channel": crate::tools::cron_schema::origin_channel_schema(),
                "origin_chat": crate::tools::cron_schema::origin_chat_schema()
            },
            "required": ["job_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.config.cron.enabled {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("cron is disabled by config (cron.enabled=false)".to_string()),
            });
        }

        let job_id = match args.get("job_id").and_then(serde_json::Value::as_str) {
            Some(v) if !v.trim().is_empty() => v,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'job_id' parameter".to_string()),
                });
            }
        };

        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Security policy: read-only mode, cannot perform 'cron_run'".into()),
            });
        }

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Rate limit exceeded: too many actions in the last hour.{}",
                    crate::tools::RATE_LIMIT_REMEDIATION
                )),
            });
        }

        let job = match cron::get_job(&self.config, job_id) {
            Ok(job) => job,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        // A chat may only run a job it created; the scope comes from the
        // turn's memory view, not from `args`. A turn with no view is
        // refused before any lookup.
        let origin_owned = match crate::tools::cron_schema::cron_origin_for_view(
            crate::memory::current_memory_view().as_ref(),
            &args,
        ) {
            Ok(o) => o,
            Err(reason) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(reason),
                });
            }
        };
        let origin_ref = origin_owned.as_ref().map(|(c, h)| (c.as_str(), h.as_str()));
        if let Err(reason) = cron::ensure_visible_to_origin(&job, origin_ref) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(reason),
            });
        }

        if matches!(job.job_type, JobType::Shell) {
            if let Err(reason) = self
                .security
                .validate_command_execution(&job.command, false)
            {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(reason),
                });
            }
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Rate limit exceeded: action budget exhausted.{}",
                    crate::tools::RATE_LIMIT_REMEDIATION
                )),
            });
        }

        let (success, output) =
            cron::scheduler::run_job_manual(&self.config, &self.security, &job, None, None).await;
        let status = if success { "ok" } else { "error" };

        Ok(ToolResult {
            success,
            output: serde_json::to_string_pretty(&json!({
                "job_id": job.id,
                "status": status,
                "output": output
            }))?,
            error: if success {
                None
            } else {
                Some("cron job execution failed".to_string())
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::memory::{MemoryView, MEMORY_VIEW};
    use crate::security::AutonomyLevel;
    use tempfile::TempDir;

    async fn test_config(tmp: &TempDir) -> Arc<Config> {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        tokio::fs::create_dir_all(&config.workspace_dir)
            .await
            .unwrap();
        Arc::new(config)
    }

    fn test_security(cfg: &Config) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::from_config(
            &cfg.autonomy,
            &cfg.workspace_dir,
        ))
    }

    /// Wraps the operator's `All` view path around the call so the existing
    /// assertions stay focused on the scheduling and approval flow. View-based
    /// refusal is asserted separately by `cron_origin_for_view`'s own
    /// tests; `no_view_run_is_refused` below covers the cron_run side.
    async fn execute_under_all_view(tool: &CronRunTool, args: serde_json::Value) -> ToolResult {
        MEMORY_VIEW
            .scope(
                MemoryView::All,
                async move { tool.execute(args).await.unwrap() },
            )
            .await
    }

    #[tokio::test]
    async fn force_runs_job_and_records_history() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo run-now").unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let result = execute_under_all_view(&tool, json!({ "job_id": job.id })).await;
        assert!(result.success, "{:?}", result.error);

        let runs = cron::list_runs(&cfg, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
    }

    #[tokio::test]
    async fn runtime_allow_grant_reaches_a_manual_run() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        // Empty boot allowlist + Supervised: only a runtime /allow grant can
        // permit the command.
        config.autonomy.allowed_commands = vec![];
        config.autonomy.level = AutonomyLevel::Supervised;
        tokio::fs::create_dir_all(&config.workspace_dir)
            .await
            .unwrap();
        let cfg = Arc::new(config);

        // Grant `true` (a real low-risk binary, no path args) at runtime on the
        // long-lived policy the tool holds. Pre-fix, the manual run built a fresh
        // policy without this grant and blocked the command; the fix threads this
        // instance through, so the grant reaches execution.
        let security = Arc::new(SecurityPolicy::from_config(
            &cfg.autonomy,
            &cfg.workspace_dir,
        ));
        security.add_runtime_command("true", false).unwrap();

        let job = cron::add_job(&cfg, "*/5 * * * *", "true").unwrap();
        let tool = CronRunTool::new(cfg.clone(), security);

        let result = execute_under_all_view(&tool, json!({ "job_id": job.id })).await;
        assert!(
            result.success,
            "a runtime /allow grant must reach the manual run: {:?} / {}",
            result.error, result.output
        );
    }

    #[tokio::test]
    async fn errors_for_missing_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let result = execute_under_all_view(&tool, json!({ "job_id": "missing-job-id" })).await;
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("not found"));
    }

    #[tokio::test]
    async fn blocks_run_in_read_only_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::ReadOnly;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo run-now").unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let result = execute_under_all_view(&tool, json!({ "job_id": job.id })).await;
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("read-only"));
    }

    /// `approved` used to be a tool parameter, which meant the *model* filled
    /// it in — a caller could grant itself the approval the gate was asking
    /// for. Emitting it now is inert.
    ///
    /// Asserts on `.error`, not `.output`: the tool gate and the scheduler gate
    /// are distinguished by which field they land in, and only the tool gate
    /// produces this message with an empty `output`.
    #[tokio::test]
    async fn cron_run_does_not_accept_an_approved_argument() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::Supervised;
        config.autonomy.allowed_commands = vec!["touch".into()];
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let job = cron::add_job(&cfg, "*/5 * * * *", "touch cron-run-smuggled").unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let out =
            execute_under_all_view(&tool, json!({ "job_id": job.id, "approved": true })).await;
        assert!(
            !out.success,
            "a model-supplied approval must not unlock the gate"
        );
        assert!(out.error.unwrap_or_default().contains("explicit approval"));
    }

    #[tokio::test]
    async fn shell_run_refuses_medium_risk_without_operator_approval() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::Supervised;
        config.autonomy.allowed_commands = vec!["touch".into()];
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let job = cron::add_job(&cfg, "*/5 * * * *", "touch cron-run-approval").unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        let denied = execute_under_all_view(&tool, json!({ "job_id": job.id })).await;
        assert!(!denied.success);
        assert!(denied
            .error
            .unwrap_or_default()
            .contains("explicit approval"));
    }

    /// A turn with no memory view cannot run any cron job — refusing here
    /// matches `cron_origin_for_view`'s contract.
    #[tokio::test]
    async fn no_view_run_is_refused() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo run-now").unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        // No MEMORY_VIEW scope at all — the helper sees None.
        let out = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(!out.success, "no view must refuse cron_run");
        assert!(out.error.unwrap_or_default().contains("no memory view"));
    }

    /// The plan's group-turn guard: under an `Only` view, a chat cannot run a
    /// job scheduled in another chat. The refusal carries no trace of the
    /// foreign job's name, so the chat cannot probe what the other chat
    /// scheduled. Control: the same view still runs the chat's own job.
    #[tokio::test]
    async fn under_an_only_view_a_foreign_job_cannot_be_run() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg));

        // chat-b creates a job with a distinctive name — the refusal must
        // not surface it.
        let job_b = cron::add_shell_job(
            &cfg,
            Some("chat-b-private-name".into()),
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo from-b",
            None,
            false,
            Some("agent-tool"),
            Some("telegram"),
            Some("chat-b"),
        )
        .unwrap();

        // chat-a asks to run chat-b's job under its own view: refused, and
        // the sentence does not carry the foreign job's name.
        let result_a = MEMORY_VIEW
            .scope(MemoryView::Only("telegram:chat-a".into()), async {
                tool.execute(json!({ "job_id": job_b.id })).await.unwrap()
            })
            .await;
        assert!(!result_a.success, "chat-a must not run chat-b's job");
        let err = result_a.error.unwrap_or_default();
        assert!(
            !err.contains("chat-b-private-name"),
            "the refusal must not reveal the job name: {err}"
        );

        // Control: chat-a creates its own job and runs it under the same
        // view. A shell job needs no provider and runs successfully, so the
        // visibility check (the one this test exercises) is the only gate
        // the call passes through.
        let job_a = cron::add_shell_job(
            &cfg,
            Some("chat-a-own-name".into()),
            crate::cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo from-a",
            None,
            false,
            Some("agent-tool"),
            Some("telegram"),
            Some("chat-a"),
        )
        .unwrap();
        let result_own = MEMORY_VIEW
            .scope(MemoryView::Only("telegram:chat-a".into()), async {
                tool.execute(json!({ "job_id": job_a.id })).await.unwrap()
            })
            .await;
        assert!(
            result_own.success,
            "chat-a must run its own job under its view: {:?} / {}",
            result_own.error, result_own.output
        );
    }
}
