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

        // The scope comes from the turn's memory view: a chat lists only its
        // own jobs, the TUI/CLI/web console lists every job, and a turn with
        // no view is refused.
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
    use crate::memory::{MemoryView, MEMORY_VIEW};
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

        // Under the unscoped `All` view (TUI / CLI / web console), an empty
        // store lists nothing.
        let result = MEMORY_VIEW
            .scope(MemoryView::All, async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
        assert!(result.success);
        assert_eq!(result.output.trim(), "[]");
    }

    #[tokio::test]
    async fn errors_when_cron_disabled() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = (*test_config(&tmp).await).clone();
        cfg.cron.enabled = false;
        let tool = CronListTool::new(Arc::new(cfg));

        let result = MEMORY_VIEW
            .scope(MemoryView::All, async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("cron is disabled"));
    }

    /// Two chats create a job each; each chat lists only its own. An
    /// un-scoped caller (CLI/TUI/console) sees both, plus the legacy
    /// origin-less rows that pre-date the scope column.
    ///
    /// The scope now comes from the turn's memory view, not from `args`. A
    /// chat runs under `Only(<surface>:<reply_target>)` and sees only its
    /// own jobs; the TUI / CLI / web console run under `All` and see every
    /// job (legacy origin-less rows included).
    #[tokio::test]
    async fn filters_jobs_to_their_origin_chat() {
        use crate::memory::{MemoryView, MEMORY_VIEW};

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
            None,
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
            None,
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
            None,
        )
        .unwrap();

        // Chat-A lists: under its `Only` view, sees only its own job.
        let list_a = MEMORY_VIEW
            .scope(MemoryView::Only("telegram:chat-a".into()), async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
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

        // Chat-B lists: under its `Only` view, sees only its own job.
        let list_b = MEMORY_VIEW
            .scope(MemoryView::Only("discord:chat-b".into()), async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
        let parsed_b: Vec<serde_json::Value> = serde_json::from_str(&list_b.output).unwrap();
        let names_b: Vec<&str> = parsed_b
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert!(names_b.contains(&"chat-b"));
        assert!(!names_b.contains(&"chat-a"));
        assert!(!names_b.contains(&"legacy"));

        // An un-scoped caller (All view) sees every job.
        let list_all = MEMORY_VIEW
            .scope(MemoryView::All, async {
                tool.execute(json!({})).await.unwrap()
            })
            .await;
        let parsed_all: Vec<serde_json::Value> = serde_json::from_str(&list_all.output).unwrap();
        let names_all: Vec<&str> = parsed_all
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert!(names_all.contains(&"chat-a"));
        assert!(names_all.contains(&"chat-b"));
        assert!(names_all.contains(&"legacy"));
    }

    /// A model that tries to widen the chat's `Only` view by passing a foreign
    /// `origin_channel` / `origin_chat` in the args must still see only the
    /// chat's own jobs. The view is the authority; the args are ignored when
    /// the view is set.
    #[tokio::test]
    async fn under_an_only_view_args_origin_is_ignored() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronListTool::new(cfg.clone());

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
            None,
        )
        .unwrap();
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
            None,
        )
        .unwrap();

        // Chat-A is the view, but the args claim chat-B. The view wins.
        let result = MEMORY_VIEW
            .scope(MemoryView::Only("telegram:chat-a".into()), async {
                tool.execute(json!({
                    "origin_channel": "discord",
                    "origin_chat": "chat-b",
                }))
                .await
                .unwrap()
            })
            .await;
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result.output).unwrap();
        let names: Vec<&str> = parsed.iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["chat-a"]);
    }

    /// A turn with no view (a webhook, an unset door) is refused before any
    /// query runs. The text names no job, so a caller cannot probe what is
    /// scheduled.
    #[tokio::test]
    async fn under_no_view_cron_list_is_refused() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronListTool::new(cfg);

        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        let err = result.error.unwrap_or_default();
        assert!(
            err.contains("no memory view"),
            "the refusal names the view, not a job: {err}"
        );
    }
}
