use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct CronRemoveTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}

impl CronRemoveTool {
    pub fn new(config: Arc<Config>, security: Arc<SecurityPolicy>) -> Self {
        Self { config, security }
    }

    fn enforce_mutation_allowed(&self, action: &str) -> Option<ToolResult> {
        if !self.security.can_act() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Security policy: read-only mode, cannot perform '{action}'"
                )),
            });
        }

        if self.security.is_rate_limited() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Rate limit exceeded: too many actions in the last hour.{}",
                    crate::tools::RATE_LIMIT_REMEDIATION
                )),
            });
        }

        if !self.security.record_action() {
            return Some(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Rate limit exceeded: action budget exhausted.{}",
                    crate::tools::RATE_LIMIT_REMEDIATION
                )),
            });
        }

        None
    }
}

#[async_trait]
impl Tool for CronRemoveTool {
    fn name(&self) -> &str {
        "cron_remove"
    }

    fn description(&self) -> &str {
        "Remove a cron job by id"
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

        if let Some(blocked) = self.enforce_mutation_allowed("cron_remove") {
            return Ok(blocked);
        }

        // A chat may only remove a job it created; the scope comes from the
        // turn's memory view, not from `args`. Un-scoped callers (TUI /
        // CLI / console) run under `All` and own every job. A turn with
        // no view is refused before any lookup.
        let job = match cron::get_job(&self.config, job_id) {
            Ok(j) => j,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };
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

        match cron::remove_job(&self.config, job_id) {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("Removed cron job {job_id}"),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            }),
        }
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

    /// Operator's `All` view path; keeps the existing assertions focused on the
    /// removal flow. View-based refusal has its own unit tests in
    /// `cron_origin_for_view`'s module.
    async fn execute_under_all_view(tool: &CronRemoveTool, args: serde_json::Value) -> ToolResult {
        MEMORY_VIEW
            .scope(
                MemoryView::All,
                async move { tool.execute(args).await.unwrap() },
            )
            .await
    }

    #[tokio::test]
    async fn removes_existing_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo ok").unwrap();
        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));

        let result = execute_under_all_view(&tool, json!({"job_id": job.id})).await;
        assert!(result.success);
        assert!(cron::list_jobs(&cfg).unwrap().is_empty());
    }

    #[tokio::test]
    async fn errors_when_job_id_missing() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));

        let result = execute_under_all_view(&tool, json!({})).await;
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("Missing 'job_id'"));
    }

    #[tokio::test]
    async fn blocks_remove_in_read_only_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::ReadOnly;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo ok").unwrap();
        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));

        let result = execute_under_all_view(&tool, json!({"job_id": job.id})).await;
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("read-only"));
    }

    /// A chat refusing a job from another chat must not confirm the job
    /// exists: the sentence names nothing but the caller's own chat. Now the
    /// scope comes from `MemoryView`, not from `args`, so this test wraps the
    /// two calls in their respective chat views.
    #[tokio::test]
    async fn removing_another_chats_job_is_refused_without_revealing_it() {
        use crate::cron::{Schedule, SessionTarget};

        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));

        // chat-b creates a job with a distinctive name.
        let job_b = cron::add_agent_job(
            &cfg,
            Some("chat-b-private-name".into()),
            Schedule::At {
                at: chrono::Utc::now() + chrono::Duration::minutes(10),
            },
            "remind B",
            SessionTarget::Isolated,
            None,
            None,
            false,
            Some("agent-tool"),
            Some("discord"),
            Some("chat-b"),
        )
        .unwrap();

        // chat-a asks to remove it under its own `Only` view: refused, and
        // the sentence does not carry the job's name.
        let result_a = MEMORY_VIEW
            .scope(MemoryView::Only("telegram:chat-a".into()), async {
                tool.execute(json!({ "job_id": job_b.id })).await.unwrap()
            })
            .await;
        assert!(!result_a.success, "chat-a must not remove chat-b's job");
        let err = result_a.error.unwrap_or_default();
        assert!(
            !err.contains("chat-b-private-name"),
            "the refusal must not reveal the job name: {err}"
        );

        // The job still exists, untouched.
        assert!(cron::get_job(&cfg, &job_b.id).is_ok());

        // The owner chat removes its own job: fine.
        let result_b = MEMORY_VIEW
            .scope(MemoryView::Only("discord:chat-b".into()), async {
                tool.execute(json!({ "job_id": job_b.id })).await.unwrap()
            })
            .await;
        assert!(result_b.success, "{:?}", result_b.error);
        assert!(cron::get_job(&cfg, &job_b.id).is_err());

        // An un-scoped caller removes any job without an origin check.
        let job_any = cron::add_job(&cfg, "*/5 * * * *", "echo ok").unwrap();
        let result = execute_under_all_view(&tool, json!({"job_id": job_any.id})).await;
        assert!(result.success, "{:?}", result.error);
    }

    /// A turn with no memory view cannot remove any cron job — refusing here
    /// matches `cron_origin_for_view`'s contract.
    #[tokio::test]
    async fn no_view_remove_is_refused() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo ok").unwrap();
        let tool = CronRemoveTool::new(cfg.clone(), test_security(&cfg));

        let out = tool.execute(json!({"job_id": job.id})).await.unwrap();
        assert!(!out.success, "no view must refuse cron_remove");
        assert!(out.error.unwrap_or_default().contains("no memory view"));
        // The job still exists.
        assert!(cron::get_job(&cfg, &job.id).is_ok());
    }
}
