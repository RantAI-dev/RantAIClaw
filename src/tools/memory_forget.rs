use super::traits::{Tool, ToolResult};
use crate::memory::Memory;
use crate::security::policy::ToolOperation;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Let the agent forget/delete a memory entry
pub struct MemoryForgetTool {
    memory: Arc<dyn Memory>,
    security: Arc<SecurityPolicy>,
}

impl MemoryForgetTool {
    pub fn new(memory: Arc<dyn Memory>, security: Arc<SecurityPolicy>) -> Self {
        Self { memory, security }
    }
}

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        "memory_forget"
    }

    fn description(&self) -> &str {
        "Remove a memory. Address it by 'key', or by 'contains' with a distinctive phrase from its content when the key is not known. Use to delete outdated facts or sensitive data."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "The key of the memory to forget"
                },
                "contains": {
                    "type": "string",
                    "description": "Alternative to 'key': a distinctive phrase from the memory's content. Must match exactly one memory — if it matches several, the call fails and names them."
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key_arg = args.get("key").and_then(|v| v.as_str());
        let contains_arg = args.get("contains").and_then(|v| v.as_str());

        // Exactly one selector. Accepting both would make it ambiguous which one
        // decides when they disagree, and that ambiguity deletes something.
        let key: String = match (key_arg, contains_arg) {
            (Some(k), None) => k.to_string(),
            (None, Some(needle)) => {
                match super::memory_store::resolve_unique_entry(
                    self.memory.as_ref(),
                    needle,
                    "contains",
                )
                .await
                {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(error),
                        })
                    }
                }
            }
            (Some(_), Some(_)) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Pass either 'key' or 'contains', not both".into()),
                })
            }
            (None, None) => return Err(anyhow::anyhow!("Missing 'key' or 'contains' parameter")),
        };
        let key = key.as_str();

        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "memory_forget")
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        match self.memory.forget(key).await {
            Ok(true) => Ok(ToolResult {
                success: true,
                output: format!("Forgot memory: {key}"),
                error: None,
            }),
            Ok(false) => Ok(ToolResult {
                success: true,
                output: format!("No memory found with key: {key}"),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to forget memory: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryCategory, SqliteMemory};
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use tempfile::TempDir;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    fn test_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryForgetTool::new(mem, test_security());
        assert_eq!(tool.name(), "memory_forget");
        assert!(tool.parameters_schema()["properties"]["key"].is_object());
    }

    #[tokio::test]
    async fn forget_existing() {
        let (_tmp, mem) = test_mem();
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security());
        let result = tool.execute(json!({"key": "temp"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Forgot"));

        assert!(mem.get("temp").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forget_nonexistent() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryForgetTool::new(mem, test_security());
        let result = tool.execute(json!({"key": "nope"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memory found"));
    }

    #[tokio::test]
    async fn forget_missing_key() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryForgetTool::new(mem, test_security());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    // ── contains selector ─────────────────────────────────────────

    #[tokio::test]
    async fn forget_by_contains_removes_the_entry() {
        let (_tmp, mem) = test_mem();
        mem.store(
            "obscure_key_9f2",
            "The staging password rotates weekly",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security());
        let result = tool
            .execute(json!({"contains": "staging password"}))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert!(mem.get("obscure_key_9f2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forget_by_ambiguous_contains_is_rejected() {
        let (_tmp, mem) = test_mem();
        mem.store("a", "the deploy runbook", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "the deploy schedule", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security());
        let result = tool.execute(json!({"contains": "deploy"})).await.unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("matches 2 memories"));
        assert!(
            mem.get("a").await.unwrap().is_some(),
            "nothing may be deleted"
        );
        assert!(mem.get("b").await.unwrap().is_some());
    }

    /// Accepting both selectors would leave it ambiguous which one decides when
    /// they disagree — and that ambiguity deletes something.
    #[tokio::test]
    async fn forget_requires_exactly_one_selector() {
        let (_tmp, mem) = test_mem();
        mem.store("k", "some content", MemoryCategory::Core, None)
            .await
            .unwrap();
        let tool = MemoryForgetTool::new(mem.clone(), test_security());

        let both = tool
            .execute(json!({"key": "k", "contains": "some"}))
            .await
            .unwrap();
        assert!(!both.success);
        assert!(both.error.unwrap_or_default().contains("not both"));

        let neither = tool.execute(json!({})).await;
        assert!(neither.is_err(), "neither selector must be an error");

        assert!(mem.get("k").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn forget_blocked_in_readonly_mode() {
        let (_tmp, mem) = test_mem();
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        let readonly = Arc::new(SecurityPolicy::default().with_autonomy(AutonomyLevel::ReadOnly));
        let tool = MemoryForgetTool::new(mem.clone(), readonly);
        let result = tool.execute(json!({"key": "temp"})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("read-only mode"));
        assert!(mem.get("temp").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn forget_blocked_when_rate_limited() {
        let (_tmp, mem) = test_mem();
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        let limited = Arc::new(SecurityPolicy::default().with_max_actions_per_hour(0));
        let tool = MemoryForgetTool::new(mem.clone(), limited);
        let result = tool.execute(json!({"key": "temp"})).await.unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Rate limit exceeded"));
        assert!(mem.get("temp").await.unwrap().is_some());
    }
}
