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
    /// Needed to re-project `MEMORY.md` after a delete. Without it the entry the
    /// agent just forgot keeps reaching the model, because the prompt injects
    /// that file and only backend construction rewrites it.
    workspace_dir: std::path::PathBuf,
}

impl MemoryForgetTool {
    pub fn new(
        memory: Arc<dyn Memory>,
        security: Arc<SecurityPolicy>,
        workspace_dir: std::path::PathBuf,
    ) -> Self {
        Self {
            memory,
            security,
            workspace_dir,
        }
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
            Ok(true) => {
                crate::memory::snapshot::refresh_projection(
                    self.memory.as_ref(),
                    &self.workspace_dir,
                );
                Ok(ToolResult {
                    success: true,
                    output: format!("Forgot memory: {key}"),
                    error: None,
                })
            }
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
        let (tmp, mem) = test_mem();
        let tool = MemoryForgetTool::new(mem, test_security(), tmp.path().to_path_buf());
        assert_eq!(tool.name(), "memory_forget");
        assert!(tool.parameters_schema()["properties"]["key"].is_object());
    }

    #[tokio::test]
    async fn forget_existing() {
        let (tmp, mem) = test_mem();
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
        let result = tool.execute(json!({"key": "temp"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Forgot"));

        assert!(mem.get("temp").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forget_nonexistent() {
        let (tmp, mem) = test_mem();
        let tool = MemoryForgetTool::new(mem, test_security(), tmp.path().to_path_buf());
        let result = tool.execute(json!({"key": "nope"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memory found"));
    }

    #[tokio::test]
    async fn forget_missing_key() {
        let (tmp, mem) = test_mem();
        let tool = MemoryForgetTool::new(mem, test_security(), tmp.path().to_path_buf());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    // ── contains selector ─────────────────────────────────────────

    #[tokio::test]
    async fn forget_by_contains_removes_the_entry() {
        let (tmp, mem) = test_mem();
        mem.store(
            "obscure_key_9f2",
            "The staging password rotates weekly",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
        let result = tool
            .execute(json!({"contains": "staging password"}))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert!(mem.get("obscure_key_9f2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forget_by_ambiguous_contains_is_rejected() {
        let (tmp, mem) = test_mem();
        mem.store("a", "the deploy runbook", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "the deploy schedule", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
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
        let (tmp, mem) = test_mem();
        mem.store("k", "some content", MemoryCategory::Core, None)
            .await
            .unwrap();
        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());

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

    // ── the projection follows the store ──────────────────────────

    /// `MEMORY.md` is injected into every system prompt, and on sqlite it is a
    /// projection of the `core` rows. Nothing re-projects on its own, so a delete
    /// that skipped it left the forgotten entry reaching the model — for the rest
    /// of the process, on the long-lived gateway and TUI.
    #[tokio::test]
    async fn forget_reprojects_memory_md() {
        let (tmp, mem) = test_mem();
        mem.store(
            "rotation_note",
            "staging credentials rotate weekly",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let projected = crate::memory::snapshot::project_core_memories(tmp.path()).unwrap();
        assert_eq!(projected, 1, "control: the projection wrote the entry");
        let before = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(
            before.contains("rotation_note"),
            "control: it is in the file"
        );

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
        let result = tool.execute(json!({"key": "rotation_note"})).await.unwrap();
        assert!(result.success, "control: the tool reports success");
        assert!(
            mem.get("rotation_note").await.unwrap().is_none(),
            "control: gone from the authoritative store"
        );

        let after = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(
            !after.contains("rotation_note"),
            "the prompt-injected file still holds the forgotten entry:\n{after}"
        );
    }

    /// The projection is a rewrite of the whole marked block, so a delete that
    /// triggers it must not take the surviving entries with it.
    #[tokio::test]
    async fn forget_leaves_the_other_projected_entries_alone() {
        let (tmp, mem) = test_mem();
        mem.store("keep", "still true", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("drop", "no longer true", MemoryCategory::Core, None)
            .await
            .unwrap();
        crate::memory::snapshot::project_core_memories(tmp.path()).unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
        tool.execute(json!({"key": "drop"})).await.unwrap();

        let after = std::fs::read_to_string(tmp.path().join("MEMORY.md")).unwrap();
        assert!(
            after.contains("keep"),
            "surviving entry was dropped:\n{after}"
        );
        assert!(!after.contains("drop"));
    }

    #[tokio::test]
    async fn forget_blocked_in_readonly_mode() {
        let (tmp, mem) = test_mem();
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        let readonly = Arc::new(SecurityPolicy::default().with_autonomy(AutonomyLevel::ReadOnly));
        let tool = MemoryForgetTool::new(mem.clone(), readonly, tmp.path().to_path_buf());
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
        let (tmp, mem) = test_mem();
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();
        let limited = Arc::new(SecurityPolicy::default().with_max_actions_per_hour(0));
        let tool = MemoryForgetTool::new(mem.clone(), limited, tmp.path().to_path_buf());
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
