use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;

const MAX_RUN_OUTPUT_CHARS: usize = 500;

pub struct CronRunsTool {
    config: Arc<Config>,
}

impl CronRunsTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[derive(Serialize)]
struct RunView {
    id: i64,
    job_id: String,
    started_at: chrono::DateTime<chrono::Utc>,
    finished_at: chrono::DateTime<chrono::Utc>,
    status: String,
    output: Option<String>,
    duration_ms: Option<i64>,
}

#[async_trait]
impl Tool for CronRunsTool {
    fn is_read_only_call(&self, _args: &serde_json::Value) -> bool {
        true
    }

    fn name(&self) -> &str {
        "cron_runs"
    }

    fn description(&self) -> &str {
        "List recent run history for a cron job"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string" },
                "limit": { "type": "integer" },
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

        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(10, |v| usize::try_from(v).unwrap_or(10));

        // A chat may only see run history of a job it created; the scope
        // comes from the turn's memory view, not from `args`. A turn with
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
        let origin_ref = origin_owned
            .as_ref()
            .map(|(c, h, t)| (c.as_str(), h.as_str(), t.as_deref()));
        if let Err(reason) = cron::ensure_visible_to_origin(&job, origin_ref) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(reason),
            });
        }

        match cron::list_runs(&self.config, job_id, limit) {
            Ok(runs) => {
                let runs: Vec<RunView> = runs
                    .into_iter()
                    .map(|run| RunView {
                        id: run.id,
                        job_id: run.job_id,
                        started_at: run.started_at,
                        finished_at: run.finished_at,
                        status: run.status,
                        output: run.output.map(|out| truncate(&out, MAX_RUN_OUTPUT_CHARS)),
                        duration_ms: run.duration_ms,
                    })
                    .collect();

                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&runs)?,
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(e.to_string()),
            }),
        }
    }
}

fn truncate(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    let mut out: String = input.chars().take(max_chars).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::memory::{MemoryView, MEMORY_VIEW};
    use chrono::{Duration as ChronoDuration, Utc};
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

    /// The operator's `All` view path, so the existing assertions stay focused
    /// on the truncation logic. The view-based refusal contract has its own
    /// unit tests in `cron_origin_for_view`'s module.
    async fn execute_under_all_view(tool: &CronRunsTool, args: serde_json::Value) -> ToolResult {
        MEMORY_VIEW
            .scope(
                MemoryView::All,
                async move { tool.execute(args).await.unwrap() },
            )
            .await
    }

    #[tokio::test]
    async fn lists_runs_with_truncation() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo ok").unwrap();

        let long_output = "x".repeat(1000);
        let now = Utc::now();
        cron::record_run(
            &cfg,
            &job.id,
            now,
            now + ChronoDuration::milliseconds(1),
            "ok",
            Some(&long_output),
            1,
        )
        .unwrap();

        let tool = CronRunsTool::new(cfg.clone());
        let result = execute_under_all_view(&tool, json!({ "job_id": job.id, "limit": 5 })).await;

        assert!(result.success);
        assert!(result.output.contains("..."));
    }

    #[tokio::test]
    async fn errors_when_job_id_missing() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronRunsTool::new(cfg);
        let result = execute_under_all_view(&tool, json!({})).await;
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("Missing 'job_id'"));
    }

    /// A turn with no memory view cannot read any run history — refusing here
    /// matches `cron_origin_for_view`'s contract.
    #[tokio::test]
    async fn no_view_runs_is_refused() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let job = cron::add_job(&cfg, "*/5 * * * *", "echo ok").unwrap();
        let tool = CronRunsTool::new(cfg);

        let out = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(!out.success, "no view must refuse cron_runs");
        assert!(out.error.unwrap_or_default().contains("no memory view"));
    }
}
