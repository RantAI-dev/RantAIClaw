//! Memory view: the task-local scope of what a turn may read from memory.
//!
//! Every door that starts an agent turn sets a view, and a turn whose view is
//! unset reads nothing. A door that forgets to set one fails closed: the
//! reader returns no entries instead of every entry.
//!
//! * `All` is a private, host-side place: the operator's console, the CLI, the
//!   TUI, a heartbeat, a named owner in a direct chat.
//! * `Only(key)` is one conversation: the channel + reply target + optional
//!   thread that `conversation_memory_scope` builds on a `ChannelMessage`. A
//!   guest, an owner in a group and a scheduled job created from a chat run
//!   under it.
//! * Unset is a place with no owner identity, such as a webhook caller.
//!
//! Modeled on `TURN_SCOPE` (`src/security/pending.rs:84-105`): a `tokio`
//! task-local set by the door around the turn, and read by every path that
//! feeds stored memory into a prompt or a tool result. `current_memory_view()`
//! returns `None` where no door set a view.

use anyhow::Result;

use super::traits::{Memory, MemoryCategory, MemoryEntry};

tokio::task_local! {
    /// The memory view the current task runs under. Outside any
    /// `MEMORY_VIEW.scope(..)` block no view is set, and every read path
    /// returns nothing. A door sets [`MemoryView::All`] for a private place or
    /// [`MemoryView::Only`] for one conversation around the turn's future, so
    /// any read inside the future follows it.
    pub static MEMORY_VIEW: MemoryView;
}

/// Per-turn memory visibility. See [`MEMORY_VIEW`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryView {
    /// No scoping: every recall returns everything the backend has. Set by
    /// the doors that are private to the operator.
    All,
    /// The recall must contain only entries whose `session_id` equals `key`
    /// (the conversation scope from `conversation_memory_scope`). Reads on
    /// backends that ignore `session_id` (markdown, lucid remote) return
    /// nothing — entries without a `session_id` are never this view's data.
    Only(String),
}

/// The view the current task runs under, or `None` when no door set one.
///
/// Every read path treats `None` as "read nothing", so a door that forgets to
/// set a view never widens a read.
#[must_use]
pub fn current_memory_view() -> Option<MemoryView> {
    MEMORY_VIEW.try_with(|v| v.clone()).ok()
}

/// The one answer `memory_store` and `memory_forget` give a turn with no view.
///
/// A turn with no view reads nothing and writes nothing, so a tool refuses it
/// before it looks anything up. The text names no key and does not depend on the
/// arguments, so the refusal is the same whether or not the key exists.
pub const NO_MEMORY_VIEW_REFUSAL: &str = "Memory is not available in this conversation.";

/// Appended to the success output of every memory-delete path (the CLI
/// `rantaiclaw memory clear` / `clear --key`, the `memory_forget` tool, the
/// gateway `memory_delete`, and the TUI `/memory remove`). The delete
/// reaches the stored note but not the conversation that previously read it
/// into a turn — that copy lives until `/new` (or its equivalent) clears
/// the chat's history. Without this line a successful delete can look
/// total to the operator and leave a stale note visible from the same
/// chat a moment later; pinning the same sentence in every path keeps
/// the operator's mental model of what was and was not removed aligned
/// across surfaces.
pub const DELETED_NOTE_HELD_BY_HISTORY: &str =
    "A conversation that mentioned the note still holds it until /new in that chat.";

/// Recall under `view`. `Only(key)` is the hard filter: every entry whose
/// `session_id` is not exactly `key` is dropped. `All` is the unchanged global
/// read.
///
/// The trait cannot ask "shared tier only", so the only way for a guest to
/// reach shared memory on a scope-capable backend is the unscoped slot, and
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

/// Delete the entry stored under `key` if the current turn's view can see it.
///
/// `All` deletes any entry. `Only(place)` deletes it only when its `session_id`
/// is exactly `place`. A task with no view deletes nothing, as it reads
/// nothing. Both refusals return `Ok(false)`, the answer a key nobody stored
/// gets, so a caller learns nothing about a row outside its view.
///
/// A host-side door (the console, the CLI, the TUI) is a private place and sets
/// `All` around the call. If one of them is ever reachable from a chat, the view
/// it runs under already limits it.
pub async fn forget_in_view(memory: &dyn Memory, key: &str) -> Result<bool> {
    match current_memory_view() {
        Some(MemoryView::All) => memory.forget(key).await,
        Some(MemoryView::Only(place)) => match memory.get(key).await? {
            Some(entry) if entry.session_id.as_deref() == Some(place.as_str()) => {
                memory.forget(key).await
            }
            _ => Ok(false),
        },
        None => Ok(false),
    }
}

/// Store a note at an explicit `session_id` on behalf of a host-side door.
///
/// Choosing the place of a note is the operator's authority, so it needs the
/// `All` view: under `Only(..)` or with no view the write is refused. The
/// agent's own `memory_store` does not come through here; it takes its place
/// from the view.
pub async fn store_in_view(
    memory: &dyn Memory,
    key: &str,
    content: &str,
    category: MemoryCategory,
    session_id: Option<&str>,
) -> Result<()> {
    match current_memory_view() {
        Some(MemoryView::All) => memory.store(key, content, category, session_id).await,
        Some(MemoryView::Only(_)) | None => {
            anyhow::bail!("choosing where a note is stored needs the operator's All memory view")
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
        // `Some(conv1)`, as the fixture does. The filter must still drop the
        // shared (no `session_id`) row and the other conversation's row.
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

    /// `All` is the unscoped read: the backend is asked for `None`, and
    /// everything it returns survives. The doors private to the operator set
    /// this view.
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
    /// outside do not. `MEMORY_VIEW.scope(..)` is the only door.
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

        let inner_result = MEMORY_VIEW
            .scope(MemoryView::Only("conv1".into()), async {
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
