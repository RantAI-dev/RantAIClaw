use super::traits::{Tool, ToolResult};
use crate::memory::{recall_in_view, Memory};
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::sync::Arc;

/// Let the agent search its own memory.
///
/// The read follows the turn's [`crate::memory::MemoryView`], which the door
/// that started the turn sets. A turn with no view finds nothing.
pub struct MemoryRecallTool {
    memory: Arc<dyn Memory>,
}

impl MemoryRecallTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Search long-term memory for relevant facts, preferences, or context. Returns scored results ranked by relevance. Scoped to the current conversation when the surface limits the turn to one; a turn that no surface gave a memory view finds nothing."
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

        // The turn's view decides what is readable. No view means no door set
        // one, and the answer is the same as for an empty store.
        let recalled = match crate::memory::current_memory_view() {
            Some(view) => recall_in_view(self.memory.as_ref(), query, limit, &view).await,
            None => Ok(Vec::new()),
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
    use crate::memory::{MemoryCategory, SessionScope, SqliteMemory};
    use tempfile::TempDir;

    fn seeded_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    /// Runs the tool the way a door that serves the operator does: under the
    /// `All` view.
    async fn execute_in_all_view(
        tool: &MemoryRecallTool,
        args: serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        crate::memory::MEMORY_VIEW
            .scope(crate::memory::MemoryView::All, tool.execute(args))
            .await
    }

    #[tokio::test]
    async fn recall_empty() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
        let result = execute_in_all_view(&tool, json!({"query": "anything"}))
            .await
            .unwrap();
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

        let tool = MemoryRecallTool::new(mem);
        let result = execute_in_all_view(&tool, json!({"query": "Rust"}))
            .await
            .unwrap();
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

        let tool = MemoryRecallTool::new(mem);
        let result = execute_in_all_view(&tool, json!({"query": "Rust", "limit": 3}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Found 3"));
    }

    #[tokio::test]
    async fn recall_missing_query() {
        let (_tmp, mem) = seeded_mem();
        let tool = MemoryRecallTool::new(mem);
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

        let tool = MemoryRecallTool::new(mem);
        let result = execute_in_all_view(&tool, json!({"query": "Rust"}))
            .await
            .unwrap();

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
        let tool = MemoryRecallTool::new(mem);
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
            scope: SessionScope<'_>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            // Encode the session scope in the same `Option<String>` shape the
            // old probe recorded: `Any` → None, `Private` → Some("__private__"),
            // `Conversation(k)` → Some(k). The two assertions below only
            // exercise `Any` and `Conversation`, but keeping `Private`
            // observable lets future probes distinguish it from `Any`.
            let recorded = match scope {
                SessionScope::Any => None,
                SessionScope::Private => Some("__private__".to_string()),
                SessionScope::Conversation(k) => Some(k.to_string()),
            };
            self.calls.lock().expect("probe mutex").push(recorded);
            Ok(vec![])
        }
        async fn get(&self, _k: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
            Ok(None)
        }
        async fn list(
            &self,
            _c: Option<&MemoryCategory>,
            _s: SessionScope<'_>,
        ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _k: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn count(&self, _scope: SessionScope<'_>) -> anyhow::Result<usize> {
            Ok(0)
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    /// A turn that no door gave a view finds nothing, and does not even ask the
    /// backend. The store holds a matching note, so an empty answer is the
    /// default doing its work and not an empty fixture.
    #[tokio::test]
    async fn a_turn_with_no_view_finds_nothing_and_does_not_read() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mem: Arc<dyn Memory> = Arc::new(RecallScopeProbe {
            calls: calls.clone(),
        });
        let tool = MemoryRecallTool::new(mem);
        let result = tool.execute(json!({"query": "anything"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("No memories found"));
        assert!(
            calls.lock().unwrap().is_empty(),
            "a turn with no view must not read the backend"
        );

        let (_tmp, stored) = seeded_mem();
        stored
            .store("lang", "User prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        let tool = MemoryRecallTool::new(stored);
        let blind = tool.execute(json!({"query": "Rust"})).await.unwrap();
        assert!(blind.output.contains("No memories found"), "{blind:?}");
        let sighted = execute_in_all_view(&tool, json!({"query": "Rust"}))
            .await
            .unwrap();
        assert!(sighted.output.contains("Found 1"), "{sighted:?}");
    }

    /// When a turn runs inside `MEMORY_VIEW.scope(Only(k), ...)` the tool reads
    /// through `recall_in_view`, which calls `recall(Some(k))`: the exact
    /// session key, never an unscoped read. The view filter in
    /// `recall_in_view` then drops anything without a matching `session_id`.
    #[tokio::test]
    async fn memory_view_only_routes_through_recall_in_view() {
        use crate::memory::{MemoryView, MEMORY_VIEW};
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mem: Arc<dyn Memory> = Arc::new(RecallScopeProbe {
            calls: calls.clone(),
        });

        let tool = MemoryRecallTool::new(mem);
        let view = MemoryView::Only("chat:abc".into());
        MEMORY_VIEW
            .scope(view, async {
                tool.execute(json!({"query": "anything"})).await.unwrap();
            })
            .await;

        let seen = calls.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![Some("chat:abc".to_string())],
            "MEMORY_VIEW = Only must route through recall_in_view (single scoped read with the view key), got {seen:?}"
        );
    }

    /// `MEMORY_VIEW = All` is the unscoped read: one recall that asks for no
    /// session.
    #[tokio::test]
    async fn memory_view_all_reads_the_whole_store() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mem: Arc<dyn Memory> = Arc::new(RecallScopeProbe {
            calls: calls.clone(),
        });

        let tool = MemoryRecallTool::new(mem);
        execute_in_all_view(&tool, json!({"query": "anything"}))
            .await
            .unwrap();

        let seen = calls.lock().unwrap().clone();
        assert_eq!(seen, vec![None], "MEMORY_VIEW = All must read globally");
    }
}
