use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct CronListTool {
    config: Arc<Config>,
}

impl CronListTool {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        "cron_list"
    }

    fn description(&self) -> &str {
        "List scheduled cron jobs. From a chat, only the jobs created in that \
         chat are visible; origin-less jobs (created from the TUI / CLI / web \
         console) are managed only there. The TUI / CLI / web console see \
         every job."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "origin_channel": crate::tools::cron_schema::origin_channel_schema(),
                "origin_chat": crate::tools::cron_schema::origin_chat_schema()
            },
            "additionalProperties": false
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

        let origin_owned = crate::tools::cron_schema::origin_filter(&args);
        let origin_ref = origin_owned.as_ref().map(|(c, h)| (c.as_str(), h.as_str()));
        match cron::list_jobs_for_origin(&self.config, origin_ref) {
            Ok(jobs) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&jobs)?,
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
    use crate::cron::{Schedule, SessionTarget};
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

    #[tokio::test]
    async fn returns_empty_list_when_no_jobs() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronListTool::new(cfg);

        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output.trim(), "[]");
    }

    #[tokio::test]
    async fn errors_when_cron_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = (*test_config(&tmp).await).clone();
        cfg.cron.enabled = false;
        let tool = CronListTool::new(Arc::new(cfg));

        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("cron is disabled"));
    }

    /// Two chats create a job each; each chat lists only its own. An
    /// un-scoped caller (CLI/TUI/console) sees both, plus the legacy
    /// origin-less rows that pre-date the scope column.
    #[tokio::test]
    async fn filters_jobs_to_their_origin_chat() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronListTool::new(cfg.clone());

        // Chat-A creates its job.
        let _ = cron::add_agent_job(
            &cfg,
            Some("chat-a".into()),
            Schedule::At {
                at: chrono::Utc::now() + chrono::Duration::minutes(10),
            },
            "remind A",
            SessionTarget::Isolated,
            None,
            None,
            false,
            Some("agent-tool"),
            Some("telegram"),
            Some("chat-a"),
        )
        .unwrap();
        // Chat-B creates its job.
        let _ = cron::add_agent_job(
            &cfg,
            Some("chat-b".into()),
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
        // A legacy origin-less row — a job written before this column
        // existed, or by a path that does not set the origin.
        let _ = cron::add_shell_job(
            &cfg,
            Some("legacy".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo legacy",
            None,
            false,
            None,
            None,
            None,
        )
        .unwrap();

        // Chat-A lists: sees only its own job.
        let list_a = tool
            .execute(json!({
                "origin_channel": "telegram",
                "origin_chat": "chat-a",
            }))
            .await
            .unwrap();
        assert!(list_a.success);
        let parsed_a: Vec<serde_json::Value> = serde_json::from_str(&list_a.output).unwrap();
        let names_a: Vec<&str> = parsed_a
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert!(
            names_a.contains(&"chat-a"),
            "chat-a must see its own: {names_a:?}"
        );
        assert!(
            !names_a.contains(&"chat-b"),
            "chat-a must NOT see chat-b: {names_a:?}"
        );
        assert!(
            !names_a.contains(&"legacy"),
            "an origin-less job is managed only from the TUI / CLI / console — a chat must not list it: {names_a:?}"
        );

        // Chat-B lists: sees only its own + legacy.
        let list_b = tool
            .execute(json!({
                "origin_channel": "discord",
                "origin_chat": "chat-b",
            }))
            .await
            .unwrap();
        let parsed_b: Vec<serde_json::Value> = serde_json::from_str(&list_b.output).unwrap();
        let names_b: Vec<&str> = parsed_b
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert!(names_b.contains(&"chat-b"));
        assert!(!names_b.contains(&"chat-a"));
        assert!(!names_b.contains(&"legacy"));

        // An un-scoped caller (no origin) sees every job.
        let list_all = tool.execute(json!({})).await.unwrap();
        let parsed_all: Vec<serde_json::Value> = serde_json::from_str(&list_all.output).unwrap();
        let names_all: Vec<&str> = parsed_all
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert!(names_all.contains(&"chat-a"));
        assert!(names_all.contains(&"chat-b"));
        assert!(names_all.contains(&"legacy"));
    }
}
