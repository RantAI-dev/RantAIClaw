use super::traits::{Tool, ToolResult};
use crate::memory::{Memory, MemoryCategory};
use crate::security::policy::ToolOperation;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Find the single stored entry whose content contains `needle`.
///
/// Shared by `memory_store`'s `replaces` and `memory_forget`'s `contains`, which
/// exist so the agent can name a memory by what it says rather than by a key it
/// would otherwise have to look up first.
///
/// Ambiguity is an error, never a guess. Both callers delete what they resolve,
/// and deleting the wrong memory silently is worse than making the caller be
/// specific. No match is an error too: the selector is a claim about existing
/// state, and quietly ignoring a false claim hides it.
pub(super) async fn resolve_unique_entry(
    memory: &dyn Memory,
    needle: &str,
    selector_name: &str,
) -> Result<String, String> {
    let needle_trimmed = needle.trim();
    if needle_trimmed.is_empty() {
        return Err(format!("'{selector_name}' must not be empty"));
    }

    let entries = memory
        .list(None, None)
        .await
        .map_err(|e| format!("Failed to read memory: {e}"))?;

    let needle_lower = needle_trimmed.to_lowercase();
    let matches: Vec<&crate::memory::MemoryEntry> = entries
        .iter()
        .filter(|e| e.content.to_lowercase().contains(&needle_lower))
        .collect();

    match matches.as_slice() {
        [one] => Ok(one.key.clone()),
        [] => Err(format!(
            "No memory contains '{needle_trimmed}', so there is nothing to {}",
            if selector_name == "replaces" {
                "replace"
            } else {
                "forget"
            }
        )),
        many => {
            let keys: Vec<&str> = many.iter().map(|e| e.key.as_str()).collect();
            Err(format!(
                "'{needle_trimmed}' matches {} memories ({}); be more specific or address one by key",
                many.len(),
                keys.join(", ")
            ))
        }
    }
}

/// Let the agent store memories — its own brain writes
pub struct MemoryStoreTool {
    memory: Arc<dyn Memory>,
    security: Arc<SecurityPolicy>,
}

impl MemoryStoreTool {
    pub fn new(memory: Arc<dyn Memory>, security: Arc<SecurityPolicy>) -> Self {
        Self { memory, security }
    }
}

#[async_trait]
impl Tool for MemoryStoreTool {
    fn name(&self) -> &str {
        "memory_store"
    }

    fn description(&self) -> &str {
        "Store a fact, preference, or note in long-term memory. Use category 'core' for permanent facts, 'daily' for session notes, 'conversation' for chat context, or a custom category name. To correct an existing memory, pass 'replaces' with a distinctive phrase from the old one so it is superseded instead of piling up beside the correction."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Unique key for this memory (e.g. 'user_lang', 'project_stack')"
                },
                "content": {
                    "type": "string",
                    "description": "The information to remember"
                },
                "category": {
                    "type": "string",
                    "description": "Memory category: 'core' (permanent), 'daily' (session), 'conversation' (chat), or a custom category name. Defaults to 'core'."
                },
                "replaces": {
                    "type": "string",
                    "description": "Optional. A distinctive phrase from an existing memory this one supersedes; that memory is removed. Must match exactly one memory — if it matches several, the call fails and names them."
                }
            },
            "required": ["key", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let key = args
            .get("key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'key' parameter"))?;

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'content' parameter"))?;

        let category = match args.get("category").and_then(|v| v.as_str()) {
            Some("core") | None => MemoryCategory::Core,
            Some("daily") => MemoryCategory::Daily,
            Some("conversation") => MemoryCategory::Conversation,
            Some(other) => MemoryCategory::Custom(other.to_string()),
        };

        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "memory_store")
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        // Resolve the superseded entry before writing anything: an unresolvable
        // `replaces` means the caller's belief about stored state is wrong, and
        // storing anyway would leave the stale memory in place beside the new one
        // — exactly the pile-up this parameter exists to prevent.
        let superseded = match args.get("replaces").and_then(|v| v.as_str()) {
            Some(needle) => {
                match resolve_unique_entry(self.memory.as_ref(), needle, "replaces").await {
                    Ok(existing_key) => Some(existing_key),
                    Err(error) => {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(error),
                        })
                    }
                }
            }
            None => None,
        };

        if let Err(e) = self
            .memory
            .store(key, content, category.clone(), None)
            .await
        {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to store memory: {e}")),
            });
        }

        let mut output = format!("Stored memory: {key}");

        if let Some(old_key) = superseded {
            // Storing under the same key already replaced it.
            if old_key != key {
                use std::fmt::Write as _;
                match self.memory.forget(&old_key).await {
                    Ok(_) => {
                        let _ = write!(output, " (superseded '{old_key}')");
                    }
                    Err(e) => {
                        let _ = write!(
                            output,
                            " (warning: stored, but could not remove superseded '{old_key}': {e})"
                        );
                    }
                }
            }
        }

        if category == MemoryCategory::Core {
            if let Some(notice) = self.core_capacity_notice().await {
                output.push('\n');
                output.push_str(&notice);
            }
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

impl MemoryStoreTool {
    /// Tell the caller when core memory has outgrown the block injected into the
    /// prompt.
    ///
    /// Hermes refuses the write at this point. That is right where the bounded
    /// file *is* the memory — over budget means the fact cannot exist. Here it
    /// is not: core memory past the budget still lives in the database and is
    /// still recallable, and only the always-injected block is bounded. Refusing
    /// would destroy a working capability to simulate a constraint this
    /// architecture does not have.
    ///
    /// So the write succeeds and the result carries the signal, which is the part
    /// that was missing — the file already says `… N more not shown`, but the
    /// agent, the one thing that could consolidate, never saw it.
    async fn core_capacity_notice(&self) -> Option<String> {
        let entries = self
            .memory
            .list(Some(&MemoryCategory::Core), None)
            .await
            .ok()?;

        let mut used = 0_usize;
        let mut injected = 0_usize;
        for entry in &entries {
            let line_chars = entry.key.chars().count() + entry.content.chars().count() + 4;
            used += line_chars;
            if used <= crate::memory::snapshot::PROJECTION_MAX_CHARS {
                injected += 1;
            }
        }

        let budget = crate::memory::snapshot::PROJECTION_MAX_CHARS;
        if used <= budget {
            return None;
        }

        let omitted = entries.len().saturating_sub(injected);
        Some(format!(
            "Note: core memory is {} characters over the {budget}-character block that is \
             injected into the prompt, so {omitted} of {} core memories are no longer \
             carried there (they remain searchable). Consider consolidating — store with \
             'replaces' to supersede an entry, or memory_forget one that is no longer true.",
            used - budget,
            entries.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::SqliteMemory;
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
        let tool = MemoryStoreTool::new(mem, test_security());
        assert_eq!(tool.name(), "memory_store");
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["key"].is_object());
        assert!(schema["properties"]["content"].is_object());
    }

    #[tokio::test]
    async fn store_core() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(json!({"key": "lang", "content": "Prefers Rust"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("lang"));

        let entry = mem.get("lang").await.unwrap();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().content, "Prefers Rust");
    }

    #[tokio::test]
    async fn store_with_category() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(json!({"key": "note", "content": "Fixed bug", "category": "daily"}))
            .await
            .unwrap();
        assert!(result.success);
    }

    #[tokio::test]
    async fn store_with_custom_category() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(
                json!({"key": "proj_note", "content": "Uses async runtime", "category": "project"}),
            )
            .await
            .unwrap();
        assert!(result.success);

        let entry = mem.get("proj_note").await.unwrap().unwrap();
        assert_eq!(entry.content, "Uses async runtime");
        assert_eq!(entry.category, MemoryCategory::Custom("project".into()));
    }

    #[tokio::test]
    async fn store_missing_key() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem, test_security());
        let result = tool.execute(json!({"content": "no key"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn store_missing_content() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem, test_security());
        let result = tool.execute(json!({"key": "no_content"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn store_blocked_in_readonly_mode() {
        let (_tmp, mem) = test_mem();
        let readonly = Arc::new(SecurityPolicy::default().with_autonomy(AutonomyLevel::ReadOnly));
        let tool = MemoryStoreTool::new(mem.clone(), readonly);
        let result = tool
            .execute(json!({"key": "lang", "content": "Prefers Rust"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("read-only mode"));
        assert!(mem.get("lang").await.unwrap().is_none());
    }

    // ── replaces / consolidation ──────────────────────────────────

    #[tokio::test]
    async fn store_with_replaces_supersedes_the_matching_entry() {
        let (_tmp, mem) = test_mem();
        mem.store(
            "old_lang",
            "The operator prefers Python",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(json!({
                "key": "user_lang",
                "content": "The operator prefers Rust",
                "replaces": "prefers Python"
            }))
            .await
            .unwrap();

        assert!(result.success, "{:?}", result.error);
        assert!(
            result.output.contains("superseded 'old_lang'"),
            "{}",
            result.output
        );
        assert!(mem.get("old_lang").await.unwrap().is_none());
        assert_eq!(
            mem.get("user_lang").await.unwrap().unwrap().content,
            "The operator prefers Rust"
        );
    }

    /// Deleting the wrong memory silently is worse than making the caller be
    /// specific, so an ambiguous selector fails and names the candidates.
    #[tokio::test]
    async fn store_with_ambiguous_replaces_is_rejected() {
        let (_tmp, mem) = test_mem();
        mem.store("a", "the deploy runbook", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "the deploy schedule", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(json!({"key": "c", "content": "new", "replaces": "deploy"}))
            .await
            .unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(error.contains("matches 2 memories"), "{error}");
        assert!(error.contains('a') && error.contains('b'), "{error}");

        assert!(
            mem.get("a").await.unwrap().is_some(),
            "nothing may be deleted"
        );
        assert!(mem.get("b").await.unwrap().is_some());
        assert!(
            mem.get("c").await.unwrap().is_none(),
            "nothing may be stored"
        );
    }

    #[tokio::test]
    async fn store_with_unmatched_replaces_is_rejected() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());

        let result = tool
            .execute(json!({"key": "k", "content": "v", "replaces": "nothing like this"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .unwrap_or_default()
            .contains("nothing to replace"));
        assert!(mem.get("k").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn core_store_reports_when_the_projection_is_over_budget() {
        let (_tmp, mem) = test_mem();
        let filler = "y".repeat(900);
        for i in 0..6 {
            mem.store(&format!("bulk_{i}"), &filler, MemoryCategory::Core, None)
                .await
                .unwrap();
        }

        let tool = MemoryStoreTool::new(mem.clone(), test_security());
        let result = tool
            .execute(json!({"key": "one_more", "content": "a durable fact"}))
            .await
            .unwrap();

        assert!(result.success, "the write must still succeed");
        assert!(
            result.output.contains("over the") && result.output.contains("consolidat"),
            "expected a capacity notice, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn core_store_is_quiet_under_the_budget() {
        let (_tmp, mem) = test_mem();
        let tool = MemoryStoreTool::new(mem.clone(), test_security());

        let result = tool
            .execute(json!({"key": "small", "content": "a durable fact"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(
            !result.output.contains("consolidat"),
            "no notice below the budget, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn store_blocked_when_rate_limited() {
        let (_tmp, mem) = test_mem();
        let limited = Arc::new(SecurityPolicy::default().with_max_actions_per_hour(0));
        let tool = MemoryStoreTool::new(mem.clone(), limited);
        let result = tool
            .execute(json!({"key": "lang", "content": "Prefers Rust"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Rate limit exceeded"));
        assert!(mem.get("lang").await.unwrap().is_none());
    }
}
