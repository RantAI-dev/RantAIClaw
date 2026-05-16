use anyhow::Result;
use std::sync::Arc;

use super::{CommandHandler, CommandResult};
use crate::memory::{Memory, MemoryCategory};
use crate::tui::context::TuiContext;

/// Bridge from the sync `CommandHandler::execute` call site to the async
/// [`Memory`] trait. Slash commands run on the TUI tokio runtime, so we
/// can mark the current thread blocking-safe via `block_in_place` and
/// drive the future to completion on the existing reactor.
///
/// Returns `Err` if no tokio runtime is current (would only happen from
/// pure unit tests that hit `MemoryCommand::execute` outside a runtime).
fn run_blocking<F, T>(future: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| anyhow::anyhow!("memory command must run inside a tokio runtime"))?;
    Ok(tokio::task::block_in_place(|| handle.block_on(future))?)
}

fn no_backend_message() -> CommandResult {
    CommandResult::Message(
        "Memory backend unavailable (running without an attached agent).".to_string(),
    )
}

fn ensure_backend(ctx: &TuiContext) -> Option<Arc<dyn Memory>> {
    ctx.memory.clone()
}

/// `/memory` command — add, list, get, recall, or remove memory entries.
pub struct MemoryCommand;

impl CommandHandler for MemoryCommand {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Inspect and manage persistent memory entries"
    }

    fn usage(&self) -> &str {
        "/memory [list|get|add|remove|recall] [args]"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let trimmed = args.trim();
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let subcmd = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();

        match subcmd {
            "" | "help" => Ok(usage_message()),
            "list" => list_memory(ctx, rest),
            "get" => get_memory(ctx, rest),
            "add" | "store" => add_memory(ctx, rest),
            "remove" | "rm" | "delete" => remove_memory(ctx, rest),
            "recall" | "search" => recall_memory(ctx, rest),
            other => Ok(CommandResult::Message(format!(
                "Unknown subcommand '{other}'. Try /memory help."
            ))),
        }
    }
}

fn usage_message() -> CommandResult {
    CommandResult::Message(
        "Usage: /memory <subcommand> [args]\n\
         \n  list [category]            list stored entries (optionally filter by category)\
         \n  get <key>                  show one entry's full content\
         \n  add <key> <content>        store a new core memory entry\
         \n  remove <key>               delete an entry by key\
         \n  recall <query> [limit]     keyword search across all entries\
         \n\nAlias: /forget <key> is shorthand for /memory remove."
            .to_string(),
    )
}

fn list_memory(ctx: &TuiContext, rest: &str) -> Result<CommandResult> {
    let Some(memory) = ensure_backend(ctx) else {
        return Ok(no_backend_message());
    };
    let category = parse_category(rest);
    let entries = run_blocking(async {
        memory
            .list(category.as_ref(), None)
            .await
            .map_err(anyhow::Error::from)
    })?;
    if entries.is_empty() {
        return Ok(CommandResult::Message(format!(
            "(no entries{} found)",
            match category {
                Some(cat) => format!(" in category '{cat}'"),
                None => String::new(),
            }
        )));
    }
    let mut out = String::new();
    out.push_str(&format!(
        "Memory entries ({}, backend: {}):\n",
        entries.len(),
        memory.name()
    ));
    for entry in entries.iter().take(50) {
        let preview = truncate_preview(&entry.content, 80);
        out.push_str(&format!(
            "  [{category}] {key}  ·  {preview}\n",
            category = entry.category,
            key = entry.key,
        ));
    }
    if entries.len() > 50 {
        out.push_str(&format!(
            "  … {} more (use `/memory recall` to filter)\n",
            entries.len() - 50
        ));
    }
    Ok(CommandResult::Message(out))
}

fn get_memory(ctx: &TuiContext, rest: &str) -> Result<CommandResult> {
    let key = rest.trim();
    if key.is_empty() {
        return Ok(CommandResult::Message(
            "Usage: /memory get <key>".to_string(),
        ));
    }
    let Some(memory) = ensure_backend(ctx) else {
        return Ok(no_backend_message());
    };
    let entry = run_blocking(async { memory.get(key).await.map_err(anyhow::Error::from) })?;
    match entry {
        Some(e) => Ok(CommandResult::Message(format!(
            "{key}\n  category: {category}\n  stored:   {ts}\n\n{content}",
            key = e.key,
            category = e.category,
            ts = e.timestamp,
            content = e.content,
        ))),
        None => Ok(CommandResult::Message(format!(
            "No entry with key '{key}'."
        ))),
    }
}

fn add_memory(ctx: &TuiContext, rest: &str) -> Result<CommandResult> {
    let mut parts = rest.splitn(2, char::is_whitespace);
    let key = parts.next().unwrap_or("").trim();
    let content = parts.next().unwrap_or("").trim();
    if key.is_empty() || content.is_empty() {
        return Ok(CommandResult::Message(
            "Usage: /memory add <key> <content>".to_string(),
        ));
    }
    let Some(memory) = ensure_backend(ctx) else {
        return Ok(no_backend_message());
    };
    // User-driven `/memory add` is global — not scoped to the
    // current session. Session-scoped entries come from the agent
    // auto-saving turn context, which uses a different path.
    let key_owned = key.to_string();
    let content_owned = content.to_string();
    run_blocking(async move {
        memory
            .store(&key_owned, &content_owned, MemoryCategory::Core, None)
            .await
            .map_err(anyhow::Error::from)
    })?;
    Ok(CommandResult::Message(format!(
        "Stored '{key}' in core memory."
    )))
}

fn remove_memory(ctx: &TuiContext, rest: &str) -> Result<CommandResult> {
    let key = rest.trim();
    if key.is_empty() {
        return Ok(CommandResult::Message(
            "Usage: /memory remove <key>".to_string(),
        ));
    }
    let Some(memory) = ensure_backend(ctx) else {
        return Ok(no_backend_message());
    };
    let key_owned = key.to_string();
    let removed =
        run_blocking(async move { memory.forget(&key_owned).await.map_err(anyhow::Error::from) })?;
    if removed {
        Ok(CommandResult::Message(format!("Forgot '{key}'.")))
    } else {
        Ok(CommandResult::Message(format!(
            "No entry with key '{key}' to remove."
        )))
    }
}

fn recall_memory(ctx: &TuiContext, rest: &str) -> Result<CommandResult> {
    let mut parts = rest.rsplitn(2, char::is_whitespace);
    let (query, limit) = if let Some(maybe_limit) = parts.next() {
        if let Ok(n) = maybe_limit.parse::<usize>() {
            let q = parts.next().unwrap_or("").trim();
            if q.is_empty() {
                (maybe_limit.trim(), 5usize)
            } else {
                (q, n.clamp(1, 50))
            }
        } else {
            (rest.trim(), 5)
        }
    } else {
        (rest.trim(), 5)
    };
    if query.is_empty() {
        return Ok(CommandResult::Message(
            "Usage: /memory recall <query> [limit]".to_string(),
        ));
    }
    let Some(memory) = ensure_backend(ctx) else {
        return Ok(no_backend_message());
    };
    let query_owned = query.to_string();
    let entries = run_blocking(async move {
        memory
            .recall(&query_owned, limit, None)
            .await
            .map_err(anyhow::Error::from)
    })?;
    if entries.is_empty() {
        return Ok(CommandResult::Message(format!(
            "No memory entries matched '{query}'."
        )));
    }
    let mut out = String::new();
    out.push_str(&format!("{} match(es) for '{}':\n", entries.len(), query));
    for entry in entries {
        let preview = truncate_preview(&entry.content, 120);
        out.push_str(&format!(
            "  [{}] {}  ·  {}\n",
            entry.category, entry.key, preview
        ));
    }
    Ok(CommandResult::Message(out))
}

fn parse_category(s: &str) -> Option<MemoryCategory> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(match s {
        "core" => MemoryCategory::Core,
        "daily" => MemoryCategory::Daily,
        "conversation" => MemoryCategory::Conversation,
        other => MemoryCategory::Custom(other.to_string()),
    })
}

fn truncate_preview(content: &str, max: usize) -> String {
    let single_line = content.replace('\n', " · ");
    let collapsed: String = single_line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(max).collect();
        format!("{truncated}…")
    }
}

/// `/forget` command — shorthand for `/memory remove`.
pub struct ForgetCommand;

impl CommandHandler for ForgetCommand {
    fn name(&self) -> &str {
        "forget"
    }

    fn description(&self) -> &str {
        "Remove a memory entry by key (alias for /memory remove)"
    }

    fn usage(&self) -> &str {
        "/forget <key>"
    }

    fn execute(&self, args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        remove_memory(ctx, args)
    }
}

/// `/compress` command — placeholder until turn-level context
/// compression is wired through the agent loop. Reports the snapshot
/// size so the user still gets actionable feedback.
pub struct CompressCommand;

impl CommandHandler for CompressCommand {
    fn name(&self) -> &str {
        "compress"
    }

    fn description(&self) -> &str {
        "Compress the current context by summarizing older messages"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let count = ctx.messages.len();
        if count < 10 {
            return Ok(CommandResult::Message(
                "Context is small enough, no compression needed.".to_string(),
            ));
        }
        // The actual summarisation requires a turn-level LLM call.
        // Tracked separately; the slash command stays informational
        // for now so users see the message count without a no-op.
        Ok(CommandResult::Message(format!(
            "Context: {count} messages in scrollback. Turn-level \
             summarisation is not yet wired — use /new to start a \
             fresh session for now."
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::traits::Memory;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    /// In-memory test backend so command tests don't touch disk.
    struct StubMemory {
        entries: Mutex<Vec<MemoryEntry>>,
    }

    impl StubMemory {
        fn new() -> Self {
            Self {
                entries: Mutex::new(Vec::new()),
            }
        }
        fn arc() -> Arc<dyn Memory> {
            Arc::new(Self::new())
        }
    }

    use crate::memory::traits::MemoryEntry;

    #[async_trait]
    impl Memory for StubMemory {
        fn name(&self) -> &str {
            "stub"
        }
        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.entries.lock().unwrap().push(MemoryEntry {
                id: key.to_string(),
                key: key.to_string(),
                content: content.to_string(),
                category,
                timestamp: "2026-05-15T00:00:00Z".to_string(),
                session_id: session_id.map(str::to_string),
                score: None,
            });
            Ok(())
        }
        async fn recall(
            &self,
            query: &str,
            limit: usize,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            let q = query.to_lowercase();
            let entries = self.entries.lock().unwrap();
            Ok(entries
                .iter()
                .filter(|e| {
                    e.content.to_lowercase().contains(&q) || e.key.to_lowercase().contains(&q)
                })
                .take(limit)
                .cloned()
                .collect())
        }
        async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.key == key)
                .cloned())
        }
        async fn list(
            &self,
            category: Option<&MemoryCategory>,
            _session_id: Option<&str>,
        ) -> anyhow::Result<Vec<MemoryEntry>> {
            let entries = self.entries.lock().unwrap();
            Ok(entries
                .iter()
                .filter(|e| category.is_none_or(|c| &e.category == c))
                .cloned()
                .collect())
        }
        async fn forget(&self, key: &str) -> anyhow::Result<bool> {
            let mut entries = self.entries.lock().unwrap();
            let before = entries.len();
            entries.retain(|e| e.key != key);
            Ok(entries.len() != before)
        }
        async fn count(&self) -> anyhow::Result<usize> {
            Ok(self.entries.lock().unwrap().len())
        }
        async fn health_check(&self) -> bool {
            true
        }
    }

    fn ctx_with_memory(mem: Arc<dyn Memory>) -> TuiContext {
        let (mut ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx.memory = Some(mem);
        ctx
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn memory_help_shows_subcommands() {
        let mut ctx = ctx_with_memory(StubMemory::arc());
        let res = MemoryCommand.execute("", &mut ctx).unwrap();
        match res {
            CommandResult::Message(msg) => {
                assert!(msg.contains("list"));
                assert!(msg.contains("add"));
                assert!(msg.contains("recall"));
            }
            _ => panic!("expected Message"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn memory_add_then_list_roundtrips() {
        let mem = StubMemory::arc();
        let mut ctx = ctx_with_memory(mem);
        MemoryCommand
            .execute("add likes-coffee yes, black", &mut ctx)
            .unwrap();
        let list = MemoryCommand.execute("list", &mut ctx).unwrap();
        match list {
            CommandResult::Message(msg) => {
                assert!(msg.contains("likes-coffee"), "got:\n{msg}");
                assert!(msg.contains("yes, black"), "got:\n{msg}");
            }
            _ => panic!("expected Message"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn memory_get_returns_full_entry() {
        let mem = StubMemory::arc();
        let mut ctx = ctx_with_memory(mem);
        MemoryCommand
            .execute("add fav-color blue with a hint of teal", &mut ctx)
            .unwrap();
        let res = MemoryCommand.execute("get fav-color", &mut ctx).unwrap();
        match res {
            CommandResult::Message(msg) => {
                assert!(msg.contains("blue with a hint of teal"), "got:\n{msg}");
                assert!(msg.contains("category: core"));
            }
            _ => panic!("expected Message"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn memory_remove_drops_entry() {
        let mem = StubMemory::arc();
        let mut ctx = ctx_with_memory(mem);
        MemoryCommand
            .execute("add doomed throwaway", &mut ctx)
            .unwrap();
        let removed = MemoryCommand.execute("remove doomed", &mut ctx).unwrap();
        match removed {
            CommandResult::Message(msg) => assert!(msg.contains("Forgot")),
            _ => panic!("expected Message"),
        }
        let after = MemoryCommand.execute("get doomed", &mut ctx).unwrap();
        match after {
            CommandResult::Message(msg) => assert!(msg.contains("No entry")),
            _ => panic!("expected Message"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn memory_recall_matches_substring() {
        let mem = StubMemory::arc();
        let mut ctx = ctx_with_memory(mem);
        MemoryCommand
            .execute("add a needle in haystack", &mut ctx)
            .unwrap();
        MemoryCommand
            .execute("add b totally unrelated", &mut ctx)
            .unwrap();
        let res = MemoryCommand.execute("recall needle", &mut ctx).unwrap();
        match res {
            CommandResult::Message(msg) => {
                assert!(msg.contains("haystack"));
                assert!(!msg.contains("totally unrelated"));
            }
            _ => panic!("expected Message"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn forget_is_alias_for_memory_remove() {
        let mem = StubMemory::arc();
        let mut ctx = ctx_with_memory(mem);
        MemoryCommand
            .execute("add temp delete-me", &mut ctx)
            .unwrap();
        let res = ForgetCommand.execute("temp", &mut ctx).unwrap();
        match res {
            CommandResult::Message(msg) => assert!(msg.contains("Forgot 'temp'")),
            _ => panic!("expected Message"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn no_backend_yields_explanatory_message() {
        let (mut ctx, _r, _e) = TuiContext::test_context();
        // Leave ctx.memory = None.
        let res = MemoryCommand.execute("list", &mut ctx).unwrap();
        match res {
            CommandResult::Message(msg) => {
                assert!(msg.contains("backend unavailable"), "got:\n{msg}");
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn compress_small_context_is_skipped() {
        let (mut ctx, _r, _e) = TuiContext::test_context();
        let res = CompressCommand.execute("", &mut ctx).unwrap();
        match res {
            CommandResult::Message(msg) => assert!(msg.contains("small enough")),
            _ => panic!("expected Message"),
        }
    }
}
