use crate::memory::{self, Memory};
use async_trait::async_trait;

#[async_trait]
pub trait MemoryLoader: Send + Sync {
    /// Build the `[Memory context]` block injected ahead of the user message.
    ///
    /// `conversation_id` is the conversation the turn's writes land in. It does
    /// not scope the read: the default loader recalls under the turn's
    /// [`memory::MemoryView`], which the door that started the turn sets, and a
    /// turn with no view recalls nothing.
    async fn load_context(
        &self,
        memory: &dyn Memory,
        user_message: &str,
        conversation_id: Option<&str>,
    ) -> anyhow::Result<memory::MemoryContext>;
}

pub struct DefaultMemoryLoader {
    limit: usize,
    min_relevance_score: f64,
}

impl Default for DefaultMemoryLoader {
    fn default() -> Self {
        Self {
            limit: 5,
            min_relevance_score: 0.6,
        }
    }
}

impl DefaultMemoryLoader {
    pub fn new(limit: usize, min_relevance_score: f64) -> Self {
        Self {
            limit: limit.max(1),
            min_relevance_score,
        }
    }
}

#[async_trait]
impl MemoryLoader for DefaultMemoryLoader {
    async fn load_context(
        &self,
        memory: &dyn Memory,
        user_message: &str,
        _conversation_id: Option<&str>,
    ) -> anyhow::Result<memory::MemoryContext> {
        // One builder, shared with the CLI loop and the channel dispatcher. This
        // path used to render its own block with no cap on entry count, entry
        // size or total size, so a large recall went into the prompt whole.
        Ok(memory::build_memory_context(
            memory,
            user_message,
            self.min_relevance_score,
            memory::MemoryContextLimits {
                max_entries: self.limit,
                ..memory::MemoryContextLimits::default()
            },
        )
        .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Memory, MemoryCategory, MemoryEntry, SessionScope};
    use std::sync::Arc;

    /// A loader built without a config scores at the same floor a config does.
    #[test]
    fn default_loader_uses_the_default_relevance_floor() {
        assert_eq!(
            DefaultMemoryLoader::default().min_relevance_score,
            crate::config::MemoryConfig::default().min_relevance_score
        );
    }

    struct MockMemory;
    struct MockMemoryWithEntries {
        entries: Arc<Vec<MemoryEntry>>,
    }

    #[async_trait]
    impl Memory for MockMemory {
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
            limit: usize,
            _scope: SessionScope<'_>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            if limit == 0 {
                return Ok(vec![]);
            }
            Ok(vec![MemoryEntry {
                id: "1".into(),
                key: "k".into(),
                content: "v".into(),
                // Curated category: `conversation` entries are
                // transcript rows and are excluded from context injection.
                category: MemoryCategory::Core,
                timestamp: "now".into(),
                session_id: None,
                score: None,
            }])
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _scope: SessionScope<'_>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn count(&self, _scope: SessionScope<'_>) -> anyhow::Result<usize> {
            Ok(0)
        }

        async fn health_check(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "mock"
        }
    }

    #[async_trait]
    impl Memory for MockMemoryWithEntries {
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
            _scope: SessionScope<'_>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(self.entries.as_ref().clone())
        }

        async fn get(&self, _key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(None)
        }

        async fn list(
            &self,
            _category: Option<&MemoryCategory>,
            _scope: SessionScope<'_>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            Ok(vec![])
        }

        async fn forget(&self, _key: &str) -> anyhow::Result<bool> {
            Ok(true)
        }

        async fn count(&self, _scope: SessionScope<'_>) -> anyhow::Result<usize> {
            Ok(self.entries.len())
        }

        async fn health_check(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "mock-with-entries"
        }
    }

    /// Runs `load_context` the way a door that serves the operator does: under
    /// the `All` view.
    async fn load_in_all_view(
        loader: &DefaultMemoryLoader,
        memory: &dyn Memory,
        message: &str,
        conversation_id: Option<&str>,
    ) -> memory::MemoryContext {
        memory::MEMORY_VIEW
            .scope(
                memory::MemoryView::All,
                loader.load_context(memory, message, conversation_id),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn default_loader_formats_context() {
        let loader = DefaultMemoryLoader::default();
        let context = load_in_all_view(&loader, &MockMemory, "hello", None)
            .await
            .block;
        assert!(context.contains("[Memory context]"));
        assert!(context.contains("- k: v"));
    }

    #[tokio::test]
    async fn default_loader_skips_legacy_assistant_autosave_entries() {
        let loader = DefaultMemoryLoader::new(5, 0.0);
        let memory = MockMemoryWithEntries {
            entries: Arc::new(vec![
                MemoryEntry {
                    id: "1".into(),
                    key: "assistant_resp_legacy".into(),
                    content: "fabricated detail".into(),
                    category: MemoryCategory::Daily,
                    timestamp: "now".into(),
                    session_id: None,
                    score: Some(0.95),
                },
                MemoryEntry {
                    id: "2".into(),
                    key: "user_fact".into(),
                    content: "User prefers concise answers".into(),
                    category: MemoryCategory::Core,
                    timestamp: "now".into(),
                    session_id: None,
                    score: Some(0.9),
                },
            ]),
        };

        let context = load_in_all_view(&loader, &memory, "answer style", None)
            .await
            .block;
        assert!(context.contains("user_fact"));
        assert!(!context.contains("assistant_resp_legacy"));
        assert!(!context.contains("fabricated detail"));
    }

    /// A turn no door gave a view recalls nothing, even when the caller names a
    /// conversation: the id says where writes land, not what may be read.
    #[tokio::test]
    async fn a_loader_turn_with_no_view_recalls_nothing() {
        let loader = DefaultMemoryLoader::default();
        let context = loader
            .load_context(&MockMemory, "hello", Some("telegram:123"))
            .await
            .unwrap();
        assert!(
            context.is_empty(),
            "no view must recall nothing: {context:?}"
        );
    }

    /// The loader follows the view and ignores the conversation id. The mock
    /// holds one unscoped entry: the `All` view reads it, an `Only` view drops
    /// it, whatever id the caller passes.
    #[tokio::test]
    async fn loader_follows_the_turns_view_and_not_the_conversation_id() {
        let loader = DefaultMemoryLoader::default();
        let all = load_in_all_view(&loader, &MockMemory, "hello", Some("telegram:123")).await;
        assert!(all.block.contains("- k: v"));

        let only = memory::MEMORY_VIEW
            .scope(
                memory::MemoryView::Only("telegram:123".into()),
                loader.load_context(&MockMemory, "hello", Some("telegram:123")),
            )
            .await
            .unwrap();
        assert!(
            only.is_empty(),
            "an unscoped entry reached an Only view: {only:?}"
        );
    }
}
