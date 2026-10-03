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

        enum Selector<'a> {
            Key(&'a str),
            Contains(&'a str),
        }

        // Exactly one selector. Accepting both would make it ambiguous which one
        // decides when they disagree, and that ambiguity deletes something.
        let selector = match (key_arg, contains_arg) {
            (Some(k), None) => Selector::Key(k),
            (None, Some(needle)) => Selector::Contains(needle),
            (Some(_), Some(_)) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Pass either 'key' or 'contains', not both".into()),
                })
            }
            (None, None) => return Err(anyhow::anyhow!("Missing 'key' or 'contains' parameter")),
        };

        // Gate before the selector is resolved, not after. `contains` reads the
        // whole store to find its target, so resolving first meant a refused call
        // did that work anyway and — on an unresolvable phrase — was answered out
        // of memory contents ("matches 2 memories (a, b)") instead of being told
        // it was refused. An agent following that answer retries a call it can
        // never complete. Argument shape is still checked above: a malformed call
        // is the model's mistake either way.
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

        // A turn with no view reads nothing and deletes nothing, so it is refused
        // before any selector is resolved. Past this point the answer would name
        // the key or say whether a phrase matched, and a caller outside every view
        // must not learn that. The checks above do not depend on stored state.
        if crate::memory::current_memory_view().is_none() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(crate::memory::NO_MEMORY_VIEW_REFUSAL.to_string()),
            });
        }

        // The turn's view decides what is deletable, whichever selector named it.
        // `Contains` resolves its target from the rows the view can list, and
        // `forget_in_view` repeats the check for the key it ends up with, so a
        // key addressed directly gets the same answer a missing key gets and
        // confirms nothing about a row outside the view.
        let key: String = match selector {
            Selector::Key(k) => k.to_string(),
            Selector::Contains(needle) => {
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
        };
        let key = key.as_str();

        match crate::memory::forget_in_view(self.memory.as_ref(), key).await {
            Ok(true) => {
                // The note is gone from `brain.db`, but the projection in
                // `MEMORY.md` is what the next owner prompt reads. Tell the
                // caller about a projection failure so the operator does not
                // learn from the next prompt that "Forgot" was a half-truth.
                if let Err(e) = crate::memory::snapshot::refresh_projection(
                    self.memory.as_ref(),
                    &self.workspace_dir,
                ) {
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Forgot the note in brain.db, but the projection did not refresh: {e}"
                        )),
                    });
                }
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
            // A backend error is a failure, not "not found": the key was not
            // shown to be absent, the store could not be read.
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    fn test_mem() -> (TempDir, Arc<dyn Memory>) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    /// Runs the tool the way a door that serves the operator does: under the
    /// `All` view.
    async fn execute_in_all_view(tool: &MemoryForgetTool, args: serde_json::Value) -> ToolResult {
        crate::memory::MEMORY_VIEW
            .scope(crate::memory::MemoryView::All, tool.execute(args))
            .await
            .unwrap()
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
        let result = execute_in_all_view(&tool, json!({"key": "temp"})).await;
        assert!(result.success);
        assert!(result.output.contains("Forgot"));

        assert!(mem.get("temp").await.unwrap().is_none());
    }

    /// When the projection fails to refresh after the note was removed from
    /// `brain.db`, the tool must report that — otherwise the owner prompt would
    /// keep showing a note the operator was told was forgotten.
    #[tokio::test]
    #[cfg(unix)]
    async fn forget_propagates_projection_failure() {
        // Make the workspace read-only AFTER the seed, so the projection's
        // `fs::write` fails while the `brain.db` write still succeeds.
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().to_path_buf();
        let mem: Arc<dyn Memory> = Arc::new(SqliteMemory::new(&workspace).unwrap());
        mem.store("temp", "temporary", MemoryCategory::Conversation, None)
            .await
            .unwrap();

        // Pre-create `MEMORY.md` so the projector writes to it; chmod the dir
        // read-only afterwards so the projection write fails.
        std::fs::write(workspace.join("MEMORY.md"), "").unwrap();
        let perms = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(&workspace, perms).unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), workspace.clone());
        let result = execute_in_all_view(&tool, json!({"key": "temp"})).await;

        // Restore the permissions so the test cleanup (TempDir drop) doesn't
        // refuse to remove files it cannot delete.
        let restore = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(&workspace, restore).unwrap();

        assert!(!result.success, "projection failure must fail the tool");
        let err = result.error.unwrap_or_default();
        assert!(
            err.contains("projection did not refresh"),
            "the error must name the projection: {err}"
        );
        // The note IS gone from `brain.db` — the projection failure is the
        // only thing left.
        assert!(mem.get("temp").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forget_nonexistent() {
        let (tmp, mem) = test_mem();
        let tool = MemoryForgetTool::new(mem, test_security(), tmp.path().to_path_buf());
        let result = execute_in_all_view(&tool, json!({"key": "nope"})).await;
        assert!(result.success);
        assert!(result.output.contains("No memory found"));
    }

    // ── guest memory-view scoping ────────────────────────

    /// Under a guest's conversation-scoped view, forgetting a key that
    /// belongs to a different conversation must answer the same "not found"
    /// message a genuinely missing key gets, and must not touch the row.
    #[tokio::test]
    async fn forget_by_key_outside_guest_view_reports_not_found_and_keeps_the_row() {
        use crate::memory::{MemoryView, MEMORY_VIEW};

        let (tmp, mem) = test_mem();
        mem.store(
            "other_conv_fact",
            "someone else's auto-save",
            MemoryCategory::Core,
            Some("chat:other"),
        )
        .await
        .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
        let result = MEMORY_VIEW
            .scope(MemoryView::Only("chat:guest".into()), async {
                tool.execute(json!({"key": "other_conv_fact"}))
                    .await
                    .unwrap()
            })
            .await;

        assert!(result.success);
        assert!(
            result.output.contains("No memory found"),
            "{}",
            result.output
        );
        assert!(
            mem.get("other_conv_fact").await.unwrap().is_some(),
            "the other conversation's row must survive"
        );
    }

    /// A backend error while resolving the guest-view guard must surface as a
    /// failure, not be swallowed into "No memory found" — that would tell a
    /// caller the key does not exist when the store simply could not be read.
    #[tokio::test]
    async fn forget_by_key_under_guest_view_reports_a_backend_error() {
        use crate::memory::{MemoryCategory, MemoryEntry, MemoryView, MEMORY_VIEW};

        struct FailingGetMemory;

        #[async_trait]
        impl Memory for FailingGetMemory {
            fn name(&self) -> &str {
                "failing-get"
            }
            async fn store(
                &self,
                _key: &str,
                _content: &str,
                _category: MemoryCategory,
                _session_id: Option<&str>,
            ) -> anyhow::Result<()> {
                Ok(())
            }
            async fn recall(
                &self,
                _query: &str,
                _limit: usize,
                _session_id: Option<&str>,
            ) -> anyhow::Result<Vec<MemoryEntry>> {
                Ok(Vec::new())
            }
            async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
                Err(anyhow::anyhow!("database is locked"))
            }
            async fn list(
                &self,
                _category: Option<&MemoryCategory>,
                _session_id: Option<&str>,
            ) -> anyhow::Result<Vec<MemoryEntry>> {
                Ok(Vec::new())
            }
            async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
            async fn count(&self) -> anyhow::Result<usize> {
                Ok(0)
            }
            async fn health_check(&self) -> bool {
                true
            }
        }

        let tmp = TempDir::new().unwrap();
        let tool = MemoryForgetTool::new(
            Arc::new(FailingGetMemory),
            test_security(),
            tmp.path().to_path_buf(),
        );
        let result = MEMORY_VIEW
            .scope(MemoryView::Only("chat:guest".into()), async {
                tool.execute(json!({"key": "some_key"})).await.unwrap()
            })
            .await;

        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Failed to forget memory"),
            "{:?}",
            result.error
        );
    }

    /// Control: a key that does belong to the guest's own conversation is
    /// still forgettable under the view.
    #[tokio::test]
    async fn forget_by_key_inside_guest_view_succeeds() {
        use crate::memory::{MemoryView, MEMORY_VIEW};

        let (tmp, mem) = test_mem();
        mem.store(
            "this_conv_fact",
            "the guest's own note",
            MemoryCategory::Core,
            Some("chat:guest"),
        )
        .await
        .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
        let result = MEMORY_VIEW
            .scope(MemoryView::Only("chat:guest".into()), async {
                tool.execute(json!({"key": "this_conv_fact"}))
                    .await
                    .unwrap()
            })
            .await;

        assert!(result.success, "{:?}", result.error);
        assert!(result.output.contains("Forgot"), "{}", result.output);
        assert!(mem.get("this_conv_fact").await.unwrap().is_none());
    }

    // ── every selector reaches only what the view can see ─────────

    const NO_VIEW_REFUSAL: &str = "Memory is not available in this conversation.";

    /// A turn no door gave a view reads nothing and deletes nothing. Whichever
    /// selector names the row, and whether it exists or not, the answer is the
    /// same refusal, so the caller learns nothing about what is stored.
    #[tokio::test]
    async fn forget_with_no_view_is_refused_with_one_text_whatever_the_selector() {
        let (tmp, mem) = test_mem();
        mem.store(
            "shared_key",
            "The staging password rotates weekly",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
        let calls = [
            ("an existing key", json!({"key": "shared_key"})),
            ("a missing key", json!({"key": "missing_key"})),
            (
                "a phrase that matches",
                json!({"contains": "staging password"}),
            ),
            (
                "a phrase that matches nothing",
                json!({"contains": "no such phrase"}),
            ),
        ];

        for (what, args) in calls {
            let result = tool.execute(args).await.unwrap();

            assert!(!result.success, "{what}");
            assert!(result.output.is_empty(), "{what}: {}", result.output);
            assert_eq!(result.error.as_deref(), Some(NO_VIEW_REFUSAL), "{what}");
        }

        assert!(
            mem.get("shared_key").await.unwrap().is_some(),
            "a turn with no view deleted a row"
        );
    }

    /// The refusal comes before any lookup: the backend is not asked anything.
    /// The answer text is what tells this layer from the one inside
    /// `forget_in_view`, which also deletes nothing but answers "not found".
    #[tokio::test]
    async fn forget_with_no_view_never_reaches_the_backend() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CallCountingMemory {
            calls: AtomicUsize,
        }

        #[async_trait]
        impl Memory for CallCountingMemory {
            fn name(&self) -> &str {
                "call-counting"
            }
            async fn store(
                &self,
                _key: &str,
                _content: &str,
                _category: MemoryCategory,
                _session_id: Option<&str>,
            ) -> anyhow::Result<()> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
            async fn recall(
                &self,
                _query: &str,
                _limit: usize,
                _session_id: Option<&str>,
            ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
            async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            }
            async fn list(
                &self,
                _category: Option<&MemoryCategory>,
                _session_id: Option<&str>,
            ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
            async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(false)
            }
            async fn count(&self) -> anyhow::Result<usize> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(0)
            }
            async fn health_check(&self) -> bool {
                true
            }
        }

        let tmp = TempDir::new().unwrap();
        let counting = Arc::new(CallCountingMemory {
            calls: AtomicUsize::new(0),
        });
        let tool =
            MemoryForgetTool::new(counting.clone(), test_security(), tmp.path().to_path_buf());

        for args in [json!({"key": "any_key"}), json!({"contains": "any phrase"})] {
            let result = tool.execute(args).await.unwrap();
            assert_eq!(result.error.as_deref(), Some(NO_VIEW_REFUSAL));
        }
        assert_eq!(
            counting.calls.load(Ordering::SeqCst),
            0,
            "a refused call reached the backend"
        );

        // Control: under the All view the same calls do reach it.
        let _ = execute_in_all_view(&tool, json!({"contains": "any phrase"})).await;
        assert!(
            counting.calls.load(Ordering::SeqCst) > 0,
            "control: a permitted call reads the store"
        );
    }

    /// Control for the case above: the `All` view reaches a shared row and a row
    /// kept in a conversation alike.
    #[tokio::test]
    async fn forget_by_key_in_the_all_view_reaches_every_place() {
        let (tmp, mem) = test_mem();
        mem.store("shared_key", "a shared note", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store(
            "scoped_key",
            "a conversation note",
            MemoryCategory::Core,
            Some("chat:one"),
        )
        .await
        .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
        for key in ["shared_key", "scoped_key"] {
            let result = execute_in_all_view(&tool, json!({ "key": key })).await;
            assert!(result.output.contains("Forgot"), "{}", result.output);
            assert!(mem.get(key).await.unwrap().is_none(), "{key} survived");
        }
    }

    /// A phrase that only a row in another place carries matches nothing under a
    /// conversation view, and that row survives. The answer is the one a phrase
    /// nobody wrote gets, so it confirms nothing about the other place.
    #[tokio::test]
    async fn forget_by_contains_outside_the_view_finds_nothing_and_keeps_the_row() {
        use crate::memory::{MemoryView, MEMORY_VIEW};

        let (tmp, mem) = test_mem();
        mem.store(
            "other_conv_fact",
            "the other conversation's lantern",
            MemoryCategory::Core,
            Some("chat:other"),
        )
        .await
        .unwrap();
        mem.store(
            "shared_fact",
            "the shared lantern",
            MemoryCategory::Core,
            None,
        )
        .await
        .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
        for phrase in ["other conversation's lantern", "shared lantern"] {
            let result = MEMORY_VIEW
                .scope(MemoryView::Only("chat:guest".into()), async {
                    tool.execute(json!({ "contains": phrase })).await.unwrap()
                })
                .await;
            assert!(!result.success, "{phrase}: {:?}", result.output);
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .contains("No memory contains"),
                "{phrase}: {:?}",
                result.error
            );
        }
        assert!(mem.get("other_conv_fact").await.unwrap().is_some());
        assert!(mem.get("shared_fact").await.unwrap().is_some());
    }

    /// Control: a phrase from a row in the view's own place resolves and deletes.
    #[tokio::test]
    async fn forget_by_contains_inside_the_view_removes_the_row() {
        use crate::memory::{MemoryView, MEMORY_VIEW};

        let (tmp, mem) = test_mem();
        mem.store(
            "this_conv_fact",
            "the guest's own lantern",
            MemoryCategory::Core,
            Some("chat:guest"),
        )
        .await
        .unwrap();

        let tool = MemoryForgetTool::new(mem.clone(), test_security(), tmp.path().to_path_buf());
        let result = MEMORY_VIEW
            .scope(MemoryView::Only("chat:guest".into()), async {
                tool.execute(json!({"contains": "own lantern"}))
                    .await
                    .unwrap()
            })
            .await;

        assert!(result.success, "{:?}", result.error);
        assert!(mem.get("this_conv_fact").await.unwrap().is_none());
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
        let result = execute_in_all_view(&tool, json!({"contains": "staging password"})).await;

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
        let result = execute_in_all_view(&tool, json!({"contains": "deploy"})).await;

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
        let result = execute_in_all_view(&tool, json!({"key": "rotation_note"})).await;
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
        execute_in_all_view(&tool, json!({"key": "drop"})).await;

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
        let result = execute_in_all_view(&tool, json!({"key": "temp"})).await;
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
        let result = execute_in_all_view(&tool, json!({"key": "temp"})).await;
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Rate limit exceeded"));
        assert!(mem.get("temp").await.unwrap().is_some());
    }

    // ── the gate covers the `contains` selector too ────────────────
    //
    // These used the `key` selector only, so the `contains` path resolved its
    // target — reading the whole store — before the policy was consulted. A
    // refused call was then answered out of memory contents rather than told it
    // was refused.

    #[tokio::test]
    async fn forget_by_contains_blocked_in_readonly_mode() {
        let (tmp, mem) = test_mem();
        mem.store("only", "the deploy runbook", MemoryCategory::Core, None)
            .await
            .unwrap();

        let readonly = Arc::new(SecurityPolicy::default().with_autonomy(AutonomyLevel::ReadOnly));
        let tool = MemoryForgetTool::new(mem.clone(), readonly, tmp.path().to_path_buf());
        let result = tool.execute(json!({"contains": "runbook"})).await.unwrap();

        assert!(!result.success);
        // A phrase that resolves cleanly: the refusal cannot be mistaken for the
        // ambiguity error shadowing it.
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("read-only mode"));
        assert!(mem.get("only").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn forget_by_ambiguous_contains_reports_the_gate_not_the_ambiguity() {
        let (tmp, mem) = test_mem();
        mem.store("a", "the deploy runbook", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "the deploy schedule", MemoryCategory::Core, None)
            .await
            .unwrap();

        let readonly = Arc::new(SecurityPolicy::default().with_autonomy(AutonomyLevel::ReadOnly));
        let tool = MemoryForgetTool::new(mem.clone(), readonly, tmp.path().to_path_buf());
        let result = tool.execute(json!({"contains": "deploy"})).await.unwrap();

        assert!(!result.success);
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains("read-only mode"),
            "refused call answered from memory contents: {error}"
        );
        assert!(
            !error.contains("be more specific"),
            "a refused caller must not be told to retry: {error}"
        );
        assert!(mem.get("a").await.unwrap().is_some());
        assert!(mem.get("b").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn forget_by_contains_blocked_when_rate_limited() {
        let (tmp, mem) = test_mem();
        mem.store("a", "the deploy runbook", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "the deploy schedule", MemoryCategory::Core, None)
            .await
            .unwrap();

        let limited = Arc::new(SecurityPolicy::default().with_max_actions_per_hour(0));
        let tool = MemoryForgetTool::new(mem.clone(), limited, tmp.path().to_path_buf());
        // An ambiguous phrase: resolving it first produced the "matches 2
        // memories" error, which is what made the limiter invisible here.
        let result = tool.execute(json!({"contains": "deploy"})).await.unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Rate limit exceeded"));
        assert!(mem.get("a").await.unwrap().is_some());
        assert!(mem.get("b").await.unwrap().is_some());
    }

    /// A refused call must not touch the store at all. `contains` resolution is a
    /// full `list()`, and doing it before the gate meant the work happened on
    /// every refused call — outside anything the rate limiter accounts for, since
    /// `enforce_tool_operation` is what records the action.
    #[tokio::test]
    async fn a_refused_forget_does_not_read_memory() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingMemory {
            reads: AtomicUsize,
        }

        #[async_trait]
        impl Memory for CountingMemory {
            fn name(&self) -> &str {
                "counting"
            }
            async fn store(
                &self,
                _key: &str,
                _content: &str,
                _category: MemoryCategory,
                _session_id: Option<&str>,
            ) -> anyhow::Result<()> {
                Ok(())
            }
            async fn recall(
                &self,
                _query: &str,
                _limit: usize,
                _session_id: Option<&str>,
            ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
            async fn get(&self, _key: &str) -> anyhow::Result<Option<crate::memory::MemoryEntry>> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            }
            async fn list(
                &self,
                _category: Option<&MemoryCategory>,
                _session_id: Option<&str>,
            ) -> anyhow::Result<Vec<crate::memory::MemoryEntry>> {
                self.reads.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            }
            async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
                Ok(false)
            }
            async fn count(&self) -> anyhow::Result<usize> {
                Ok(0)
            }
            async fn health_check(&self) -> bool {
                true
            }
        }

        let tmp = TempDir::new().unwrap();
        let counting = Arc::new(CountingMemory {
            reads: AtomicUsize::new(0),
        });
        let readonly = Arc::new(SecurityPolicy::default().with_autonomy(AutonomyLevel::ReadOnly));
        let tool = MemoryForgetTool::new(counting.clone(), readonly, tmp.path().to_path_buf());

        let result = tool.execute(json!({"contains": "anything"})).await.unwrap();
        assert!(!result.success);
        assert_eq!(
            counting.reads.load(Ordering::SeqCst),
            0,
            "a refused call read the store"
        );

        // Control: the same call under a permitting policy does read it, so the
        // counter is wired to something that actually happens.
        let tool =
            MemoryForgetTool::new(counting.clone(), test_security(), tmp.path().to_path_buf());
        let _ = execute_in_all_view(&tool, json!({"contains": "anything"})).await;
        assert!(
            counting.reads.load(Ordering::SeqCst) > 0,
            "control: a permitted call resolves the selector"
        );
    }
}
