use super::traits::{Tool, ToolResult};
use crate::memory::{recall_in_view, Memory, MemoryView};
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;

/// Let the agent search its own memory
/// Shared handle through which a surface points this tool at the active
/// conversation. `None` (the default) recalls globally — the behaviour every
/// surface had before scoping. The `Agent` writes it from
/// `set_conversation_id`, so the tool follows the same per-request scope the
/// injection path uses; surfaces that serve many conversations concurrently
/// through one registry (channels, the gateway webhook) leave it unset — a
/// single shared slot would race across concurrent turns and mis-scope reads,
/// which is worse than a global read.
pub type ConversationScope = std::sync::Arc<std::sync::Mutex<Option<String>>>;

pub struct MemoryRecallTool {
    memory: Arc<dyn Memory>,
    scope: ConversationScope,
}

impl MemoryRecallTool {
    pub fn new(memory: Arc<dyn Memory>, scope: ConversationScope) -> Self {
        Self { memory, scope }
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Search long-term memory for relevant facts, preferences, or context. Returns scored results ranked by relevance. Scoped to the current conversation plus shared memory when the surface provides a conversation; global otherwise."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords or phrase to search for in memory"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default: 5)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'query' parameter"))?;

        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map_or(5, |v| v as usize);

        // Scope the read to the active conversation when a surface has set
        // one: conversation-local rows first, shared unscoped tier as
        // backfill, other conversations' rows filtered — the same layered
        // read the injection path uses. Unset ⇒ global, as before.
        //
        // Plan 450 adds a per-turn override: when the dispatch runs a guest
        // turn it sets `MEMORY_VIEW = Some(MemoryView::Only(conversation))`,
        // and that view overrides the tool's own scope slot. A guest must
        // never reach the unscoped backfill even if the channel happened to
        // set a different scope earlier in the conversation lifecycle.
        let recalled = match crate::memory::current_memory_view() {
            // Only the restricted `Only` view overrides the scope slot. `All`
            // means "no restriction", so it defers to the tool's own layered
            // read below, exactly as an unset view does.
            Some(view @ MemoryView::Only(_)) => {
                recall_in_view(self.memory.as_ref(), query, limit, &view).await
            }
            _ => {
                let scope = self
                    .scope
                    .lock()
                    .map(|guard| guard.clone())
                    .unwrap_or_default();
                match scope.as_deref() {
                    Some(cid) => {
                        crate::memory::recall_layered(self.memory.as_ref(), query, limit, Some(cid))
                            .await
                    }
                    None => self.memory.recall(query, limit, None).await,
                }
            }
        };
        match recalled {
            Ok(entries) if entries.is_empty() => Ok(ToolResult {
                success: true,
                output: "No memories found matching that query.".into(),
                error: None,
            }),
            Ok(entries) => {
                let mut output = format!("Found {} memories:\n", entries.len());
                for entry in &entries {
                    // Scores are absolute relevance in [0,1]. Formatting the
                    // fraction directly as a percentage rendered a strong
                    // match as "[1%]" and everything else as "[0%]".
                    let score = entry
                        .score
                        .map_or_else(String::new, |s| format!(" [{:.0}%]", s * 100.0));
                    let _ = writeln!(
                        output,
                        "- [{}] {}: {}{score}",
                        entry.category, entry.key, entry.content
                    );
                }
                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Memory recall failed: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryCategory, SqliteMemory};
    use tempfile::TempDir;

    fn seeded_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    #[tokio::test]
    async fn recall_empty() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem, ConversationScope::default());
        let result = tool.execute(json!({"query": "anything"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("No memories found"));
    }

    #[tokio::test]
    async fn recall_finds_match() {
        let (_tmp, mem) = seeded_mem();
        mem.store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("tz", "Timezone is EST", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = MemoryRecallTool::new(mem, ConversationScope::default());
        let result = tool.execute(json!({"query": "Rust"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Rust"));
        assert!(result.output.contains("Found 1"));
    }

    #[tokio::test]
    async fn recall_respects_limit() {
        let (_tmp, mem) = seeded_mem();
        for i in 0..10 {
            mem.store(
                &format!("k{i}"),
                &format!("Rust fact {i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }

        let tool = MemoryRecallTool::new(mem, ConversationScope::default());
        let result = tool
            .execute(json!({"query": "Rust", "limit": 3}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 3"));
    }

    #[tokio::test]
    async fn recall_missing_query() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem, ConversationScope::default());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    /// Scores are relevance in [0,1]. Printing the fraction straight as a
    /// percentage rendered the best match as "[1%]".
    #[tokio::test]
    async fn recall_renders_the_score_as_a_real_percentage() {
        let (_tmp, mem) = seeded_mem();
        mem.store(
            "lang",
            "the operator prefers Rust",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let tool = MemoryRecallTool::new(mem, ConversationScope::default());
        let result = tool.execute(json!({"query": "Rust"})).await.unwrap();

        // Scores are absolute now — the exact value depends on the BM25
        // magnitude, not on being the best of the set. Pin the rendering shape
        // and that a real single-term match reads as a substantial percentage,
        // not the fraction-as-percent bug ("[1%]").
        let pct: u32 = result
            .output
            .rsplit('[')
            .next()
            .and_then(|s| s.split('%').next())
            .and_then(|s| s.parse().ok())
            .expect("a percentage in the output");
        assert!(
            (5..=100).contains(&pct),
            "expected a substantial percentage, got {pct}% in: {}",
            result.output
        );
    }

    #[test]
    fn name_and_schema() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem, ConversationScope::default());
        assert_eq!(tool.name(), "memory_recall");
        assert!(tool.parameters_schema()["properties"]["query"].is_object());
    }

    /// Records the `session_id` of every `recall` call, so a test can prove
    /// which scope the tool actually read under.
    struct RecallScopeProbe {
        calls: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    #[async_trait::async_trait]
    impl Memory for RecallScopeProbe {
        fn name(&self) -> &str {
            "recall-scope-probe"
        }
        async fn store(
            &self,
            _k: &str,
            _c: &str,
            _cat: MemoryCategory,
            _s: Option<&str>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn recall(
            &self,
            _q: &str,
            _l: usize,
            session_id: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            self.calls
                .lock()
                .expect("probe mutex")
                .push(session_id.map(str::to_string));
            Ok(vec![])
        }
        async fn get(&self, _k: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            Ok(None)
        }
        async fn list(
            &self,
            _c: Option<&MemoryCategory>,
            _s: Option<&str>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _k: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    /// With a conversation set, the tool reads through `recall_layered`:
    /// the conversation's own rows first, then the shared unscoped backfill —
    /// never a bare global read that would cross into other conversations.
    #[tokio::test]
    async fn a_set_conversation_scopes_the_tools_read() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mem: Arc<dyn Memory> = Arc::new(RecallScopeProbe {
            calls: calls.clone(),
        });
        let scope = ConversationScope::default();
        *scope.lock().unwrap() = Some("tui:s1".into());

        let tool = MemoryRecallTool::new(mem, scope);
        tool.execute(json!({"query": "anything"})).await.unwrap();

        let seen = calls.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![Some("tui:s1".to_string()), None],
            "expected the layered read: scoped first, shared backfill second"
        );
    }

    /// The control: no conversation set ⇒ exactly the old single global read.
    #[tokio::test]
    async fn an_unset_scope_reads_globally_as_before() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mem: Arc<dyn Memory> = Arc::new(RecallScopeProbe {
            calls: calls.clone(),
        });
        let tool = MemoryRecallTool::new(mem, ConversationScope::default());
        tool.execute(json!({"query": "anything"})).await.unwrap();

        let seen = calls.lock().unwrap().clone();
        assert_eq!(seen, vec![None]);
    }

    /// Plan 450: when a turn runs inside `MEMORY_VIEW.scope(Only(k), ...)`
    /// the tool reads through `recall_in_view`, which calls `recall(Some(k))`
    /// (the exact session key, never the stale scope slot, never the unscoped
    /// backfill). The view filter in `recall_in_view` then drops anything
    /// without a matching `session_id`.
    #[tokio::test]
    async fn memory_view_only_routes_through_recall_in_view() {
        use crate::memory::{MemoryView, MEMORY_VIEW};
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mem: Arc<dyn Memory> = Arc::new(RecallScopeProbe {
            calls: calls.clone(),
        });
        let scope = ConversationScope::default();
        // A different scope than the view: the view must win.
        *scope.lock().unwrap() = Some("stale-scope".into());

        let tool = MemoryRecallTool::new(mem, scope);
        let view = MemoryView::Only("chat:abc".into());
        MEMORY_VIEW
            .scope(view, async {
                tool.execute(json!({"query": "anything"})).await.unwrap();
            })
            .await;

        // The view routes through `recall_in_view`, which under the hood
        // calls `recall(Some("chat:abc"))` — a single read scoped to the
        // view key, not the tool's stale scope. The probe records exactly
        // that single argument.
        let seen = calls.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![Some("chat:abc".to_string())],
            "MEMORY_VIEW = Only must route through recall_in_view (single scoped read with the view key), got {seen:?}"
        );
    }

    /// Plan 450: `MEMORY_VIEW = All` does NOT change today's behavior —
    /// the tool still routes through the scope slot (layered read with the
    /// tool's stored conversation, then shared unscoped backfill). This
    /// documents the intent so future readers know the view is "off" unless
    /// explicitly set to `Only`.
    #[tokio::test]
    async fn memory_view_all_falls_back_to_scope() {
        use crate::memory::{MemoryView, MEMORY_VIEW};
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mem: Arc<dyn Memory> = Arc::new(RecallScopeProbe {
            calls: calls.clone(),
        });
        let scope = ConversationScope::default();
        *scope.lock().unwrap() = Some("chat:abc".into());

        let tool = MemoryRecallTool::new(mem, scope);
        let view = MemoryView::All;
        MEMORY_VIEW
            .scope(view, async {
                tool.execute(json!({"query": "anything"})).await.unwrap();
            })
            .await;

        let seen = calls.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![Some("chat:abc".to_string()), None],
            "MEMORY_VIEW = All must defer to the scope slot (existing layered read), got {seen:?}"
        );
    }
}
