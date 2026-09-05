use super::traits::{Tool, ToolResult};
use crate::config::Config;
use crate::cron::{self, DeliveryConfig, JobType, Schedule, SessionTarget};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

pub struct CronAddTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
}

impl CronAddTool {
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
impl Tool for CronAddTool {
    fn name(&self) -> &str {
        "cron_add"
    }

    fn description(&self) -> &str {
        "Schedule a task, reminder, or recurring/repeating message — THE tool for any \
'remind me', 'schedule', 'every morning/day/hour', 'daily', 'each week', or 'in N \
minutes' request (this powers the Schedules feature). \
Create a scheduled cron job (shell or agent) with cron/at/every schedules. \
For an agent job whose output should be SENT to the user, set `delivery` = \
{mode:'announce', channel, to}; the channel system prompt provides the correct \
channel + address when the request comes from a chat channel. Without `delivery`, \
the job still runs on schedule but its output is only recorded in run history \
(visible in the Schedules view) — it is NOT pushed anywhere. If the user asks to \
be messaged but you have no delivery address (e.g. the web console or TUI, which \
have no push channel), say the output will appear in the Schedules run history, or \
ask which configured channel to deliver to — do not imply a message will arrive."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "schedule": crate::tools::cron_schema::schedule_schema(),
                "job_type": { "type": "string", "enum": ["shell", "agent"] },
                "command": { "type": "string" },
                "prompt": { "type": "string" },
                "session_target": { "type": "string", "enum": ["isolated", "main"] },
                "model": { "type": "string" },
                "delivery": crate::tools::cron_schema::delivery_schema(),
                "delete_after_run": { "type": "boolean" }
            },
            "required": ["schedule"]
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

        let schedule = match args.get("schedule") {
            Some(v) => match crate::tools::cron_schema::parse_schedule(v) {
                Ok(schedule) => schedule,
                Err(reason) => {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(reason),
                    });
                }
            },
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Missing 'schedule' parameter".to_string()),
                });
            }
        };

        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        let job_type = match args.get("job_type").and_then(serde_json::Value::as_str) {
            Some("agent") => JobType::Agent,
            Some("shell") => JobType::Shell,
            Some(other) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid job_type: {other}")),
                });
            }
            None => {
                if args.get("prompt").is_some() {
                    JobType::Agent
                } else {
                    JobType::Shell
                }
            }
        };

        let result = match job_type {
            JobType::Shell => {
                let command = match args.get("command").and_then(serde_json::Value::as_str) {
                    Some(command) if !command.trim().is_empty() => command,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'command' for shell job".to_string()),
                        });
                    }
                };

                if let Err(reason) = self.security.validate_command_execution(command, false) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(reason),
                    });
                }

                // The origin-chat safety net injects a `delivery` block into any
                // cron_add from an announce-capable channel regardless of job
                // type, so a shell job ("run this and message me") must honor it
                // too — mirror the agent branch's parse + delete-after logic.
                let delivery = match args.get("delivery") {
                    Some(v) => match serde_json::from_value::<DeliveryConfig>(v.clone()) {
                        Ok(cfg) => Some(cfg),
                        Err(e) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid delivery config: {e}")),
                            });
                        }
                    },
                    None => None,
                };
                let delivered = delivery
                    .as_ref()
                    .is_some_and(|d| d.mode.eq_ignore_ascii_case("announce"));
                let default_delete_after_run = matches!(schedule, Schedule::At { .. }) && delivered;
                let delete_after_run = args
                    .get("delete_after_run")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(default_delete_after_run);

                if let Some(blocked) = self.enforce_mutation_allowed("cron_add") {
                    return Ok(blocked);
                }

                cron::add_shell_job(
                    &self.config,
                    name,
                    schedule,
                    command,
                    delivery,
                    delete_after_run,
                    Some("agent-tool"),
                )
            }
            JobType::Agent => {
                let prompt = match args.get("prompt").and_then(serde_json::Value::as_str) {
                    Some(prompt) if !prompt.trim().is_empty() => prompt,
                    _ => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some("Missing 'prompt' for agent job".to_string()),
                        });
                    }
                };

                let session_target = match args.get("session_target") {
                    Some(v) => match serde_json::from_value::<SessionTarget>(v.clone()) {
                        Ok(target) => target,
                        Err(e) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid session_target: {e}")),
                            });
                        }
                    },
                    None => SessionTarget::Isolated,
                };

                let model = args
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);

                let delivery = match args.get("delivery") {
                    Some(v) => match serde_json::from_value::<DeliveryConfig>(v.clone()) {
                        Ok(cfg) => Some(cfg),
                        Err(e) => {
                            return Ok(ToolResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid delivery config: {e}")),
                            });
                        }
                    },
                    None => None,
                };

                // Auto-delete a fired one-shot only when its output was delivered
                // to the user another way (announce delivery). Without delivery,
                // the ONLY record of the output is the run-history row — deleting
                // the job would cascade that row away (cron_runs FK ON DELETE
                // CASCADE), so keep+disable instead. An explicit `delete_after_run`
                // in the args still overrides.
                let delivered = delivery
                    .as_ref()
                    .is_some_and(|d| d.mode.eq_ignore_ascii_case("announce"));
                let default_delete_after_run = matches!(schedule, Schedule::At { .. }) && delivered;
                let delete_after_run = args
                    .get("delete_after_run")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(default_delete_after_run);

                if let Some(blocked) = self.enforce_mutation_allowed("cron_add") {
                    return Ok(blocked);
                }

                // Provenance audit trail. Never log the raw prompt — it may carry
                // injected content or secrets; record its length only.
                tracing::info!(
                    prompt_len = prompt.len(),
                    has_delivery = delivery.is_some(),
                    "cron_add: creating a scheduled agent job"
                );

                cron::add_agent_job(
                    &self.config,
                    name,
                    schedule,
                    prompt,
                    session_target,
                    model,
                    delivery,
                    delete_after_run,
                    Some("agent-tool"),
                )
            }
        };

        match result {
            Ok(job) => Ok(ToolResult {
                success: true,
                output: serde_json::to_string_pretty(&json!({
                    "id": job.id,
                    "name": job.name,
                    "job_type": job.job_type,
                    "schedule": job.schedule,
                    "next_run": job.next_run,
                    "enabled": job.enabled
                }))?,
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

    /// The reported failure: a model sent `every_ms` as `"600000"` and the tool
    /// refused. There was no machine-readable type for the field, so a provider
    /// doing structured decoding had nothing to constrain against — the model
    /// was guessing, not disobeying.
    #[tokio::test]
    async fn every_ms_accepts_a_stringified_integer() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "every", "every_ms": "600000" },
                "job_type": "shell",
                "command": "echo ok"
            }))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
    }

    /// The coerced value must be the SAME schedule the integer form produces —
    /// a tolerance that quietly changed the interval would be worse than the
    /// rejection it replaces.
    #[tokio::test]
    async fn a_stringified_every_ms_produces_the_same_schedule_as_the_integer() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let mut ids = Vec::new();
        for value in [json!(600_000), json!("600000")] {
            let result = tool
                .execute(json!({
                    "schedule": { "kind": "every", "every_ms": value },
                    "job_type": "shell",
                    "command": "echo ok"
                }))
                .await
                .unwrap();
            assert!(result.success, "{:?}", result.error);
            let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
            ids.push(v["id"].as_str().unwrap().to_string());
        }

        let a = crate::cron::get_job(&cfg, &ids[0]).unwrap();
        let b = crate::cron::get_job(&cfg, &ids[1]).unwrap();
        assert_eq!(a.schedule, b.schedule);
        assert_eq!(
            a.schedule,
            crate::cron::Schedule::Every { every_ms: 600_000 }
        );
    }

    /// Tolerating `"600000"` must not tolerate nonsense, and the refusal has to
    /// be readable by the thing that failed: serde's raw message tells a model
    /// nothing about what to send instead.
    #[tokio::test]
    async fn a_schedule_it_cannot_read_is_refused_with_an_example() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        // Zero is refused too, but by `crate::cron::schedule`, which owns
        // schedule validation and has its own message. Only the values that
        // fail to PARSE go through the new error path.
        for bad in [json!("ten minutes"), json!(-1), json!(0)] {
            let result = tool
                .execute(json!({
                    "schedule": { "kind": "every", "every_ms": bad },
                    "job_type": "shell",
                    "command": "echo ok"
                }))
                .await
                .unwrap();

            assert!(!result.success, "{bad} must be refused");
            let err = result.error.unwrap_or_default();
            assert!(err.contains("every_ms"), "must name the field: {err}");
        }

        for unparseable in [json!("ten minutes"), json!(-1)] {
            let result = tool
                .execute(json!({
                    "schedule": { "kind": "every", "every_ms": unparseable },
                    "job_type": "shell",
                    "command": "echo ok"
                }))
                .await
                .unwrap();

            let err = result.error.unwrap_or_default();
            assert!(
                err.contains(r#"{"kind": "every", "every_ms": 600000}"#),
                "a refusal a model cannot act on is the defect: {err}"
            );
        }
    }

    /// The advertised schema is the only contract a model can actually read.
    /// Asserting on it here is what stops it drifting from `Schedule` — when the
    /// enum gains a variant, this test is what forces the schema to follow.
    #[tokio::test]
    async fn the_advertised_schema_types_every_ms_as_an_integer() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let schema = tool.parameters_schema();
        let schedule = &schema["properties"]["schedule"];
        let branches = schedule["oneOf"]
            .as_array()
            .unwrap_or_else(|| panic!("schedule must declare its variants: {schedule}"));
        assert_eq!(branches.len(), 3, "one branch per Schedule variant");

        let every = branches
            .iter()
            .find(|b| b["properties"]["every_ms"].is_object())
            .unwrap_or_else(|| panic!("no `every` branch: {schedule}"));
        assert_eq!(every["properties"]["every_ms"]["type"], "integer");
        assert_eq!(every["properties"]["every_ms"]["minimum"], 1);

        // Every variant carries its own `kind` discriminator — the enum is
        // internally tagged, so the tag belongs inside each branch.
        for b in branches {
            assert!(
                b["properties"]["kind"].is_object(),
                "each branch must pin `kind`: {b}"
            );
        }

        // `delivery` was a bare object with no description at all — a model had
        // no way to learn that `announce` is what actually sends anything.
        let delivery = &schema["properties"]["delivery"];
        assert!(
            delivery["properties"]["mode"].is_object(),
            "delivery must declare its fields: {delivery}"
        );
        assert_eq!(delivery["properties"]["mode"]["enum"][0], "announce");
        for field in ["channel", "to", "best_effort"] {
            assert!(
                delivery["properties"][field].is_object(),
                "delivery must declare `{field}`: {delivery}"
            );
        }
    }

    #[tokio::test]
    async fn adds_shell_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));
        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "shell",
                "command": "echo ok"
            }))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("next_run"));
    }

    #[tokio::test]
    async fn agent_oneshot_without_delivery_keeps_run_history() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let at = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
        let result = tool
            .execute(json!({
                "schedule": { "kind": "at", "at": at },
                "job_type": "agent",
                "prompt": "remind me"
            }))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);

        let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let id = v["id"].as_str().unwrap();
        let job = crate::cron::get_job(&cfg, id).unwrap();
        assert!(
            !job.delete_after_run,
            "a no-delivery agent one-shot must NOT auto-delete (would cascade away its run history)"
        );
    }

    #[tokio::test]
    async fn agent_oneshot_with_announce_delivery_still_auto_deletes() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let at = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
        let result = tool
            .execute(json!({
                "schedule": { "kind": "at", "at": at },
                "job_type": "agent",
                "prompt": "remind me",
                "delivery": { "mode": "announce", "channel": "telegram", "to": "123" }
            }))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);

        let v: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let id = v["id"].as_str().unwrap();
        let job = crate::cron::get_job(&cfg, id).unwrap();
        assert!(
            job.delete_after_run,
            "an announce-delivery one-shot should still auto-delete (output already reached the user)"
        );
    }

    #[tokio::test]
    async fn blocks_disallowed_shell_command() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.allowed_commands = vec!["echo".into()];
        config.autonomy.level = AutonomyLevel::Supervised;
        tokio::fs::create_dir_all(&config.workspace_dir)
            .await
            .unwrap();
        let cfg = Arc::new(config);
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "shell",
                "command": "curl https://example.com"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("not allowed"));
    }

    #[tokio::test]
    async fn blocks_mutation_in_read_only_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.level = AutonomyLevel::ReadOnly;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "shell",
                "command": "echo ok"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("read-only") || error.contains("not allowed"));
    }

    #[tokio::test]
    async fn medium_risk_shell_command_is_refused() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.autonomy.allowed_commands = vec!["touch".into()];
        config.autonomy.level = AutonomyLevel::Supervised;
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let cfg = Arc::new(config);
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let denied = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "shell",
                "command": "touch cron-approval-test"
            }))
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(denied
            .error
            .unwrap_or_default()
            .contains("explicit approval"));
    }

    #[tokio::test]
    async fn rejects_invalid_schedule() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "every", "every_ms": 0 },
                "job_type": "shell",
                "command": "echo nope"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("every_ms must be > 0"));
    }

    #[tokio::test]
    async fn agent_job_requires_prompt() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronAddTool::new(cfg.clone(), test_security(&cfg));

        let result = tool
            .execute(json!({
                "schedule": { "kind": "cron", "expr": "*/5 * * * *" },
                "job_type": "agent"
            }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("Missing 'prompt'"));
    }
}
