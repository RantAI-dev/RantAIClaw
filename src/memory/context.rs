//! The single builder for the `[Memory context]` block injected ahead of a
//! user message.
//!
//! There used to be three: one in the agent's memory loader, one in the CLI
//! loop, one in the channel dispatcher. They drifted. Only the channel version
//! bounded what it produced, so the TUI, the gateway and the CLI could inject
//! an unbounded amount of recalled text into a prompt, and only the channel
//! version skipped conversation-history keys. One builder means one set of
//! rules to reason about — and one place to fix the next thing that is wrong
//! with them.

use super::{recall_layered, Memory};
use crate::util::truncate_with_ellipsis;

/// How much recalled memory may enter a prompt.
///
/// Bounds are a feature, not a safety net: an unbounded block crowds out the
/// conversation it is supposed to support, and the cost is paid on every turn.
#[derive(Debug, Clone, Copy)]
pub struct MemoryContextLimits {
    /// Most entries to include, however many were recalled.
    pub max_entries: usize,
    /// Longest a single entry may be before it is truncated.
    pub max_entry_chars: usize,
    /// Ceiling on the whole block.
    pub max_total_chars: usize,
}

impl Default for MemoryContextLimits {
    fn default() -> Self {
        Self {
            max_entries: 4,
            max_entry_chars: 800,
            max_total_chars: 4_000,
        }
    }
}

/// Entries that must never reach the prompt, whatever they scored.
fn should_skip(key: &str, content: &str, limits: &MemoryContextLimits) -> bool {
    // Model-authored summaries from a legacy auto-save. Treated as untrusted
    // context: re-injecting them lets an old fabrication harden into a fact.
    if super::is_assistant_autosave_key(key) {
        return true;
    }

    // Serialised conversation history. It is not a fact about the user, and it
    // duplicates what the transcript already carries.
    if key.trim().to_ascii_lowercase().ends_with("_history") {
        return true;
    }

    // A single entry larger than the whole budget cannot be usefully truncated
    // into it.
    content.chars().count() > limits.max_total_chars
}

/// What a turn's recall produced: the block to inject, and which entries are
/// in it.
///
/// The keys are carried separately rather than parsed back out of `block`
/// because the block's shape is a prompt-formatting decision — a reader that
/// re-derived the keys from it would break the next time that shape changed.
/// They exist so a surface can tell the operator that memory was used, and
/// which memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryContext {
    /// The rendered `[Memory context]` block. Empty when nothing survived.
    pub block: String,
    /// Keys of the entries in `block`, in the order they appear.
    pub keys: Vec<String>,
}

impl MemoryContext {
    /// True when nothing survived recall and filtering.
    pub fn is_empty(&self) -> bool {
        self.block.is_empty()
    }
}

/// Recall for this turn and render the `[Memory context]` block.
///
/// Returns an empty block when nothing survives — callers concatenate it, so
/// an empty result must cost nothing.
///
/// `conversation_id` scopes the recall through [`recall_layered`]: the
/// conversation's own memory surfaces first, then shared memory backfills.
/// `None` recalls globally.
pub async fn build_memory_context(
    memory: &dyn Memory,
    user_message: &str,
    min_relevance_score: f64,
    conversation_id: Option<&str>,
    limits: MemoryContextLimits,
) -> MemoryContext {
    let recall_limit = limits.max_entries.max(1) + 1;
    let entries = match recall_layered(memory, user_message, recall_limit, conversation_id).await {
        Ok(entries) => entries,
        Err(e) => {
            // Recall failing is not the same as recalling nothing. Callers
            // cannot tell the difference from an empty string, so say it here.
            tracing::warn!("memory recall failed while building context: {e}");
            return MemoryContext::default();
        }
    };

    let mut context = String::new();
    let mut keys: Vec<String> = Vec::new();
    let mut included = 0_usize;
    let mut used_chars = 0_usize;

    for entry in entries.iter().filter(|e| match e.score {
        Some(score) => score >= min_relevance_score,
        // Backends that do not rank still have something to say.
        None => true,
    }) {
        if included >= limits.max_entries {
            break;
        }
        if should_skip(&entry.key, &entry.content, &limits) {
            continue;
        }

        let content = if entry.content.chars().count() > limits.max_entry_chars {
            truncate_with_ellipsis(&entry.content, limits.max_entry_chars)
        } else {
            entry.content.clone()
        };

        let line = format!("- {}: {}\n", entry.key, content);
        let line_chars = line.chars().count();

        if used_chars + line_chars > limits.max_total_chars {
            break;
        }

        if included == 0 {
            context.push_str("[Memory context]\n");
        }
        context.push_str(&line);
        keys.push(entry.key.clone());
        used_chars += line_chars;
        included += 1;
    }

    if included > 0 {
        context.push('\n');
    }
    MemoryContext {
        block: context,
        keys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{MemoryCategory, MemoryEntry};
    use async_trait::async_trait;

    struct FixedMemory {
        entries: Vec<MemoryEntry>,
    }

    #[async_trait]
    impl Memory for FixedMemory {
        fn name(&self) -> &str {
            "fixed"
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
            limit: usize,
            _s: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            let mut out = self.entries.clone();
            out.truncate(limit);
            Ok(out)
        }
        async fn get(&self, _k: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }
        async fn list(
            &self,
            _c: Option<&MemoryCategory>,
            _s: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _k: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(self.entries.len())
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    fn entry(key: &str, content: &str, score: f64) -> MemoryEntry {
        MemoryEntry {
            id: key.into(),
            key: key.into(),
            content: content.into(),
            category: MemoryCategory::Core,
            timestamp: "t".into(),
            session_id: None,
            score: Some(score),
        }
    }

    fn memory_of(entries: Vec<MemoryEntry>) -> FixedMemory {
        FixedMemory { entries }
    }

    #[tokio::test]
    async fn renders_entries_above_the_threshold() {
        let mem = memory_of(vec![entry("lang", "prefers Rust", 1.0)]);
        let out = build_memory_context(&mem, "q", 0.4, None, MemoryContextLimits::default())
            .await
            .block;
        assert!(out.starts_with("[Memory context]\n"));
        assert!(out.contains("- lang: prefers Rust"));
        assert!(out.ends_with("\n\n"));
    }

    #[tokio::test]
    async fn empty_when_everything_is_below_the_threshold() {
        let mem = memory_of(vec![entry("weak", "barely related", 0.1)]);
        let out = build_memory_context(&mem, "q", 0.4, None, MemoryContextLimits::default())
            .await
            .block;
        assert!(out.is_empty(), "expected no block at all, got {out:?}");
    }

    /// The bound is the point of the shared builder: before it, the agent and
    /// CLI paths injected however many entries recall returned.
    #[tokio::test]
    async fn caps_the_number_of_entries() {
        let mem = memory_of(
            (0..10)
                .map(|i| entry(&format!("k{i}"), "fact", 1.0))
                .collect(),
        );
        let limits = MemoryContextLimits {
            max_entries: 3,
            ..MemoryContextLimits::default()
        };
        let out = build_memory_context(&mem, "q", 0.0, None, limits)
            .await
            .block;
        assert_eq!(out.lines().filter(|l| l.starts_with("- ")).count(), 3);
    }

    #[tokio::test]
    async fn truncates_an_oversized_entry() {
        let mem = memory_of(vec![entry("long", &"x".repeat(500), 1.0)]);
        let limits = MemoryContextLimits {
            max_entry_chars: 50,
            ..MemoryContextLimits::default()
        };
        let out = build_memory_context(&mem, "q", 0.0, None, limits)
            .await
            .block;
        assert!(out.contains("..."), "expected an ellipsis, got {out:?}");
        assert!(out.chars().count() < 200);
    }

    #[tokio::test]
    async fn stops_at_the_total_budget() {
        let mem = memory_of(
            (0..10)
                .map(|i| entry(&format!("k{i}"), &"y".repeat(60), 1.0))
                .collect(),
        );
        let limits = MemoryContextLimits {
            max_entries: 10,
            max_entry_chars: 100,
            max_total_chars: 150,
        };
        let out = build_memory_context(&mem, "q", 0.0, None, limits)
            .await
            .block;
        assert!(
            out.chars().count() <= 150 + "[Memory context]\n\n".len(),
            "block exceeded its budget: {} chars",
            out.chars().count()
        );
    }

    #[tokio::test]
    async fn skips_legacy_assistant_autosave_and_history_keys() {
        let mem = memory_of(vec![
            entry("assistant_resp_old", "a fabricated detail", 1.0),
            entry("telegram_123_history", "serialised transcript", 1.0),
            entry("user_fact", "prefers concise answers", 1.0),
        ]);
        let out = build_memory_context(&mem, "q", 0.0, None, MemoryContextLimits::default())
            .await
            .block;
        assert!(out.contains("user_fact"));
        assert!(!out.contains("fabricated"));
        assert!(!out.contains("transcript"));
    }

    /// The keys are what a surface shows the operator, so they must name
    /// exactly the entries that reached the prompt — not what recall returned
    /// before filtering, and not what the block happens to look like.
    #[tokio::test]
    async fn keys_name_exactly_the_entries_in_the_block() {
        let mem = memory_of(vec![
            entry("kept_a", "first fact", 1.0),
            entry("assistant_resp_old", "a fabrication", 1.0),
            entry("chat_history", "transcript", 1.0),
            entry("kept_b", "second fact", 1.0),
            entry("too_weak", "barely related", 0.1),
        ]);
        let limits = MemoryContextLimits {
            max_entries: 5,
            ..MemoryContextLimits::default()
        };
        let ctx = build_memory_context(&mem, "q", 0.4, None, limits).await;

        assert_eq!(ctx.keys, vec!["kept_a", "kept_b"]);
        for key in &ctx.keys {
            assert!(ctx.block.contains(key.as_str()), "{key} missing from block");
        }
        assert!(!ctx.block.contains("assistant_resp_old"));
        assert!(!ctx.block.contains("too_weak"));
    }

    #[tokio::test]
    async fn keys_are_empty_when_nothing_survives() {
        let mem = memory_of(vec![entry("weak", "barely related", 0.1)]);
        let ctx = build_memory_context(&mem, "q", 0.4, None, MemoryContextLimits::default()).await;
        assert!(ctx.is_empty());
        assert!(ctx.keys.is_empty(), "no block means no keys");
    }

    /// Truncation caps the block; the keys must be capped with it, or a surface
    /// would name entries the model never saw.
    #[tokio::test]
    async fn keys_stop_where_the_budget_does() {
        let mem = memory_of(
            (0..10)
                .map(|i| entry(&format!("k{i}"), "fact", 1.0))
                .collect(),
        );
        let limits = MemoryContextLimits {
            max_entries: 3,
            ..MemoryContextLimits::default()
        };
        let ctx = build_memory_context(&mem, "q", 0.0, None, limits).await;
        assert_eq!(ctx.keys.len(), 3);
        assert_eq!(
            ctx.keys.len(),
            ctx.block.lines().filter(|l| l.starts_with("- ")).count()
        );
    }

    #[tokio::test]
    async fn keeps_unscored_entries() {
        let mut e = entry("k", "v", 0.0);
        e.score = None;
        let mem = memory_of(vec![e]);
        let out = build_memory_context(&mem, "q", 0.9, None, MemoryContextLimits::default())
            .await
            .block;
        assert!(out.contains("- k: v"), "unscored entries must survive");
    }
}
