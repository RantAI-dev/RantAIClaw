//! Memory view — task-local scope of what `memory_recall` and the dispatch
//! memory-context injection may see.
//!
//! A guest's turn must see only its own conversation's memory: not the owner's
//! profile (`USER.md`), the owner's notes (`MEMORY.md`), or any other chat's
//! stored exchanges. `MemoryView` is the per-turn switch that enforces it.
//!
//! Modeled on `TURN_SCOPE` (`src/security/pending.rs:84-105`): a `tokio`
//! task-local set by the channel dispatch around the tool loop, and read by
//! the read paths that need to honour it. `current_memory_view()` returns
//! `None` everywhere it is unset, so the TUI, the CLI and tests run with
//! today's behaviour.
//!
//! `Only(key)` is the conversation scope (channel + reply_target + optional
//! thread — the same value `conversation_memory_scope` builds on a `ChannelMessage`).
//! `All` is the unscoped view every non-channel surface already has; step 5
//! of the memory effort starts setting it on the TUI.

use anyhow::Result;

use super::traits::{Memory, MemoryEntry};

tokio::task_local! {
    /// The memory view the current task runs under. `None` is the default
    /// outside any channel turn — the read paths treat it as [`MemoryView::All`]
    /// so today's behaviour is unchanged for the TUI, CLI, console, cron and
    /// webhook. Set to `Some(MemoryView::Only(scope))` around a guest's tool
    /// loop so any read inside the future is scoped to that conversation.
    pub static MEMORY_VIEW: MemoryView;
}

/// Per-turn memory visibility. See [`MEMORY_VIEW`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryView {
    /// No scoping: every recall returns everything the backend has. The
    /// pre-plan default for owners, TUI, CLI, console, cron, webhook.
    All,
    /// The recall must contain only entries whose `session_id` equals `key`
    /// (the conversation scope from `conversation_memory_scope`). Reads on
    /// backends that ignore `session_id` (markdown, lucid remote) return
    /// nothing — entries without a `session_id` are never this view's data.
    Only(String),
}

impl MemoryView {
    /// Run `future` with this view installed on the current task. Task-local:
    /// every `.await` inside the future reads `current_memory_view()` and gets
    /// `Some(self.clone())`. Outer reads (e.g. before this future is spawned)
    /// are unchanged.
    pub async fn scope<F>(self, future: F) -> F::Output
    where
        F: std::future::Future,
    {
        MEMORY_VIEW.scope(self, future).await
    }
}

/// The view the current task runs under, or `None` when no view is set.
///
/// Outside any `MEMORY_VIEW.scope(...)` block this returns `None`, which every
/// read path treats as "today's unscoped behaviour" — so a missing view never
/// silently widens a guest's read.
#[must_use]
pub fn current_memory_view() -> Option<MemoryView> {
    MEMORY_VIEW.try_with(|v| v.clone()).ok()
}

/// Recall under `view`. `Only(key)` is the hard filter: every entry whose
/// `session_id` is not exactly `key` is dropped. `All` is the unchanged global
/// read.
///
/// The trait cannot ask "shared tier only", so the only way for a guest to
/// reach shared memory on a scope-capable backend is the unsocped slot — and
/// that is exactly the slot the `Only` filter discards. A guest on markdown
/// (no `session_id` on its entries) sees nothing; that is the plan:
/// conversation-only, by construction.
pub async fn recall_in_view(
    memory: &dyn Memory,
    query: &str,
    limit: usize,
    view: &MemoryView,
) -> Result<Vec<MemoryEntry>> {
    match view {
        MemoryView::All => memory.recall(query, limit, None).await,
        MemoryView::Only(key) => {
            let results = memory.recall(query, limit, Some(key.as_str())).await?;
            Ok(results
                .into_iter()
                .filter(|e| e.session_id.as_deref() == Some(key.as_str()))
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryCategory;
    use std::sync::Mutex;

    /// Records every `recall` call so a test can prove which scope the read
    /// asked for. Returns ALL entries regardless of the slot it was asked for
    /// — i.e. mimics a backend that ignores `session_id` (markdown, lucid
    /// remote), the case this view's filter must save us from. The `None`
    /// case keeps the "no filter = all" contract the production backends
    /// share.
    #[derive(Default)]
    struct ScopeRecordingMemory {
        calls: Mutex<Vec<Option<String>>>,
        entries: Vec<MemoryEntry>,
    }

    #[async_trait::async_trait]
    impl Memory for ScopeRecordingMemory {
        fn name(&self) -> &str {
            "scope-recording"
        }
        async fn store(
            &self,
            _k: &str,
            _c: &str,
            _cat: MemoryCategory,
            _sid: Option<&str>,
        ) -> Result<()> {
            Ok(())
        }
        async fn recall(
            &self,
            _query: &str,
            limit: usize,
            session_id: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            self.calls
                .lock()
                .unwrap()
                .push(session_id.map(str::to_string));
            let mut out: Vec<MemoryEntry> = self.entries.clone();
            // Cap the unsocped case at `limit`; keep the scoped case
            // un-capped so a `Some(key)` recall never quietly loses entries
            // before the view's filter runs.
            if session_id.is_none() {
                out.truncate(limit);
            }
            Ok(out)
        }
        async fn get(&self, _k: &str) -> Result<Option<MemoryEntry>> {
            Ok(None)
        }
        async fn list(
            &self,
            _c: Option<&MemoryCategory>,
            _s: Option<&str>,
        ) -> Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _k: &str) -> Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> Result<usize> {
            Ok(self.entries.len())
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    fn entry(key: &str, content: &str, session_id: Option<&str>) -> MemoryEntry {
        MemoryEntry {
            id: key.into(),
            key: key.into(),
            content: content.into(),
            category: MemoryCategory::Core,
            timestamp: "t".into(),
            session_id: session_id.map(str::to_string),
            score: None,
        }
    }

    /// `Only(this)` must keep only entries whose `session_id` equals `this`,
    /// even when the backend returns them through the unscoped backfill.
    /// This is the test the rest of the plan lives on: a guest's
    /// `memory_recall` and injection must not see someone else's auto-save
    /// row or the owner's `MEMORY.md` projection.
    #[tokio::test]
    async fn recall_in_view_only_keeps_entries_with_the_exact_session_id() {
        let mem = ScopeRecordingMemory {
            entries: vec![
                entry("shared_fact", "lives in the shared tier", None),
                entry(
                    "this_conv_fact",
                    "lives in this conversation",
                    Some("conv1"),
                ),
                entry("other_conv_fact", "someone else's auto-save", Some("conv2")),
            ],
            ..Default::default()
        };

        // The backend is allowed to hand back all entries when called with
        // `Some(conv1)` — that is what the layered read does today. The filter
        // must still drop the shared (no `session_id`) row and the other
        // conversation's row.
        mem.calls.lock().unwrap().clear();
        let got = recall_in_view(&mem, "q", 10, &MemoryView::Only("conv1".into()))
            .await
            .unwrap();
        let keys: Vec<&str> = got.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, vec!["this_conv_fact"]);

        // And it had to ask the backend with the right session slot, or the
        // filter would silently work on whatever the backend chose to return.
        let calls = mem.calls.lock().unwrap().clone();
        assert_eq!(calls, vec![Some("conv1".to_string())]);
    }

    /// A view whose key has no matching entries — including the case where
    /// every entry is unscoped (the markdown / lucid-remote shape) — must
    /// return nothing. Returning the unscoped rows would be "shared tier
    /// leaks through a guest's view", which is the bug this whole plan fixes.
    #[tokio::test]
    async fn recall_in_view_only_drops_unscoped_entries() {
        let mem = ScopeRecordingMemory {
            entries: vec![
                entry("note_a", "markdown entry, no session", None),
                entry("note_b", "another markdown entry", None),
            ],
            ..Default::default()
        };

        let got = recall_in_view(&mem, "q", 10, &MemoryView::Only("conv1".into()))
            .await
            .unwrap();
        assert!(
            got.is_empty(),
            "Only(view) must not return unscoped entries: {got:?}"
        );
    }

    /// `All` is today's unscoped read: the backend is asked for `None`, and
    /// everything it returns survives. Owners and the TUI never get the view
    /// set, so they fall to this path — it must keep behaving like before.
    #[tokio::test]
    async fn recall_in_view_all_is_a_plain_unscoped_recall() {
        let mem = ScopeRecordingMemory {
            entries: vec![
                entry("a", "fact", None),
                entry("b", "fact", Some("conv1")),
                entry("c", "fact", Some("conv2")),
            ],
            ..Default::default()
        };

        mem.calls.lock().unwrap().clear();
        let got = recall_in_view(&mem, "q", 10, &MemoryView::All)
            .await
            .unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(
            mem.calls.lock().unwrap().clone(),
            vec![None],
            "All must call recall(.., None) — the global slot"
        );
    }

    /// The task-local semantics. Reads inside the scope see the view; reads
    /// outside do not. `MEMORY_VIEW::scope` is the only door.
    #[tokio::test]
    async fn scope_sets_the_view_for_inner_tasks_only() {
        let mem = ScopeRecordingMemory::default();
        assert!(
            current_memory_view().is_none(),
            "no view is set before .scope()"
        );

        let mem_for_outer = &mem;
        let outer_result = async {
            let view = current_memory_view();
            assert!(view.is_none(), "outer reads see no view");
            recall_in_view(mem_for_outer, "q", 10, &MemoryView::All)
                .await
                .unwrap()
        }
        .await;

        let inner_result = MemoryView::Only("conv1".into())
            .scope(async {
                let view = current_memory_view();
                assert_eq!(
                    view,
                    Some(MemoryView::Only("conv1".into())),
                    "inner reads see the scoped view"
                );
                recall_in_view(&mem, "q", 10, &view.unwrap()).await.unwrap()
            })
            .await;

        // Both calls happened.
        assert_eq!(mem.calls.lock().unwrap().len(), 2);
        let _ = (outer_result, inner_result);
    }
}
