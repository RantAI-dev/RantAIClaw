use super::traits::{Memory, MemoryCategory};
use super::{
    classify_memory_backend, create_memory_for_cli, effective_memory_backend_name,
    MemoryBackendKind,
};
use crate::config::Config;
#[cfg(feature = "memory-postgres")]
use anyhow::Context;
use anyhow::{bail, Result};
use console::style;

/// Handle `rantaiclaw memory <subcommand>` CLI commands.
pub async fn handle_command(command: crate::MemoryCommands, config: &Config) -> Result<()> {
    match command {
        crate::MemoryCommands::List {
            category,
            session,
            limit,
            offset,
        } => handle_list(config, category, session, limit, offset).await,
        crate::MemoryCommands::Get { key } => handle_get(config, &key).await,
        crate::MemoryCommands::Add {
            key,
            content,
            category,
        } => handle_add(config, &key, &content, &category).await,
        crate::MemoryCommands::Recall { query, limit } => {
            handle_recall(config, &query, limit).await
        }
        crate::MemoryCommands::Reindex => handle_reindex(config).await,
        crate::MemoryCommands::Stats => handle_stats(config).await,
        crate::MemoryCommands::Clear { key, category, yes } => {
            handle_clear(config, key, category, yes).await
        }
    }
}

/// Create a lightweight memory backend for CLI management operations.
///
/// CLI commands (list/get/stats/clear) never use vector search, so we skip
/// embedding provider initialisation for local backends by using the
/// migration factory.  Postgres still needs its full connection config.
fn create_cli_memory(config: &Config) -> Result<Box<dyn Memory>> {
    let backend = effective_memory_backend_name(
        &config.memory.backend,
        Some(&config.storage.provider.config),
    );

    match classify_memory_backend(&backend) {
        MemoryBackendKind::None => {
            bail!("Memory backend is 'none' (disabled). No entries to manage.");
        }
        MemoryBackendKind::Postgres => {
            #[cfg(not(feature = "memory-postgres"))]
            bail!("memory backend 'postgres' requires the 'memory-postgres' feature to be enabled at compile time");
            #[cfg(feature = "memory-postgres")]
            {
                let sp = &config.storage.provider.config;
                let db_url = sp
                    .db_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .context(
                        "memory backend 'postgres' requires db_url in [storage.provider.config]",
                    )?;
                let mem = super::PostgresMemory::new(
                    db_url,
                    &sp.schema,
                    &sp.table,
                    sp.connect_timeout_secs,
                )?;
                Ok(Box::new(mem))
            }
        }
        _ => create_memory_for_cli(&backend, &config.workspace_dir),
    }
}

async fn handle_list(
    config: &Config,
    category: Option<String>,
    session: Option<String>,
    limit: usize,
    offset: usize,
) -> Result<()> {
    let mem = create_cli_memory(config)?;
    let cat = category.as_deref().map(parse_category);
    let entries = mem.list(cat.as_ref(), session.as_deref()).await?;

    if entries.is_empty() {
        println!("No memory entries found.");
        return Ok(());
    }

    // `list` is capped by the backend, so its length is a page size, not a
    // total. Asking `count()` is what stops a database of 5,000 entries
    // reporting "1000 total" forever.
    let listed = entries.len();
    let total = mem.count().await.unwrap_or(listed);
    let page: Vec<_> = entries.into_iter().skip(offset).take(limit).collect();

    if page.is_empty() {
        println!("No entries at offset {offset} (total: {total}).");
        return Ok(());
    }

    let truncated = if total > listed {
        format!(" — listing the most recent {listed}")
    } else {
        String::new()
    };
    println!(
        "Memory entries ({total} total{truncated}, showing {}-{}):\n",
        offset + 1,
        offset + page.len(),
    );

    for entry in &page {
        println!(
            "- {} [{}]",
            style(&entry.key).white().bold(),
            entry.category,
        );
        println!("    {}", truncate_content(&entry.content, 80));
    }

    if offset + page.len() < total {
        println!("\n  Use --offset {} to see the next page.", offset + limit);
    }

    Ok(())
}

async fn handle_get(config: &Config, key: &str) -> Result<()> {
    let mem = create_cli_memory(config)?;

    // Try exact match first.
    if let Some(entry) = mem.get(key).await? {
        print_entry(&entry);
        return Ok(());
    }

    // Fall back to prefix match so users can copy partial keys from `list`.
    let all = mem.list(None, None).await?;
    let matches: Vec<_> = all.iter().filter(|e| e.key.starts_with(key)).collect();

    match matches.len() {
        0 => println!("No memory entry found for key: {key}"),
        1 => print_entry(matches[0]),
        n => {
            println!("Prefix '{key}' matched {n} entries:\n");
            for entry in matches {
                println!(
                    "- {} [{}]",
                    style(&entry.key).white().bold(),
                    entry.category
                );
            }
            println!("\nSpecify a longer prefix to narrow the match.");
        }
    }

    Ok(())
}

fn print_entry(entry: &super::traits::MemoryEntry) {
    println!("Key:       {}", style(&entry.key).white().bold());
    println!("Category:  {}", entry.category);
    println!("Timestamp: {}", entry.timestamp);
    if let Some(sid) = &entry.session_id {
        println!("Session:   {sid}");
    }
    println!("\n{}", entry.content);
}

/// Refresh the `MEMORY.md` projection after a write.
///
/// The agent, gateway and TUI project when they construct memory. The CLI does
/// not build memory that way — it skips embedding setup — so `memory add` and
/// `memory clear` used to leave the file that the system prompt injects
/// untouched, and a workspace seeded entirely from the command line reached the
/// model with an empty core tier until something else ran.
///
/// Delegates to `snapshot::refresh_projection`, which every other write path now
/// shares. The backend gate moved with it: gating on `mem.name()` is the same
/// decision this used to make from config, because `create_cli_memory` builds the
/// instance from exactly that classification.
fn refresh_projection(mem: &dyn Memory, config: &Config) {
    super::snapshot::refresh_projection(mem, &config.workspace_dir);
}

/// Store a memory from the command line.
///
/// The CLI could read and delete but not write, so an operator seeding a fresh
/// workspace had to go through the agent or hand-edit the database.
///
/// Screened like every other write path — this is the fourth, and the screen is
/// only worth anything if none of them skips it.
async fn handle_add(config: &Config, key: &str, content: &str, category: &str) -> Result<()> {
    let mem = create_cli_memory(config)?;

    let sanitized =
        super::sanitize_memory_content(content).map_err(|reason| anyhow::anyhow!("{reason}"))?;
    for note in &sanitized.notes {
        println!("  {} {note}", style("note:").yellow());
    }

    mem.store(key, &sanitized.content, parse_category(category), None)
        .await?;
    println!("Stored {} [{}]", style(key).white().bold(), category);
    refresh_projection(&*mem, config);
    Ok(())
}

/// Search memory from the command line.
///
/// The other three surfaces could search; this one could not, so an operator
/// checking what the agent knows had to page through `list`.
async fn handle_recall(config: &Config, query: &str, limit: usize) -> Result<()> {
    let mem = create_cli_memory(config)?;
    let hits = mem.recall(query, limit.max(1), None).await?;

    if hits.is_empty() {
        println!("No memory entries matched '{query}'.");
        return Ok(());
    }

    println!("{} match(es) for '{}':\n", hits.len(), query);
    for entry in &hits {
        // Scores are relevance relative to the best hit in this set, so render
        // them as a percentage of it rather than as a bare fraction.
        let relevance = entry
            .score
            .map_or_else(String::new, |s| format!("  [{:.0}%]", s * 100.0));
        println!(
            "- {} [{}]{relevance}",
            style(&entry.key).white().bold(),
            entry.category
        );
        println!("    {}", truncate_content(&entry.content, 80));
    }
    Ok(())
}

/// Re-embed memories the live embedding model cannot use.
///
/// Two kinds accumulate and never clear on their own: rows written while the
/// embedding provider was unavailable, and rows embedded by a previous model —
/// vector search skips the latter because a vector of another dimensionality is
/// not comparable, so switching models silently emptied it.
async fn handle_reindex(config: &Config) -> Result<()> {
    let backend = effective_memory_backend_name(
        &config.memory.backend,
        Some(&config.storage.provider.config),
    );
    if !matches!(
        classify_memory_backend(&backend),
        MemoryBackendKind::Sqlite | MemoryBackendKind::Lucid
    ) {
        bail!("memory backend '{backend}' does not store embeddings; nothing to reindex");
    }

    let mem = super::create_sqlite_with_embedder(
        &config.memory,
        &config.embedding_routes,
        &config.workspace_dir,
        config.api_key.as_deref(),
    )?;

    println!("Reindexing memory (backend: sqlite)…");
    let count = mem.reindex().await?;

    if count == 0 {
        println!("Nothing to re-embed — every memory matches the current embedding model.");
    } else {
        println!(
            "Re-embedded {} {}.",
            style(count).white().bold(),
            if count == 1 { "memory" } else { "memories" }
        );
    }
    Ok(())
}

async fn handle_stats(config: &Config) -> Result<()> {
    let mem = create_cli_memory(config)?;
    let healthy = mem.health_check().await;
    // Not `unwrap_or(0)`. `health_check` does not cover this — sqlite's is
    // `SELECT 1` and survives a damaged `memories` table, markdown's is
    // `workspace_dir.exists()` — so a store that cannot be counted printed
    // `Total: 0` next to `Health: healthy` and read as an empty store. `stats` is
    // the command an operator runs *because* memory is misbehaving; report what
    // can be read and name what cannot.
    let counted = mem.count().await;

    println!("Memory Statistics:\n");
    println!("  Backend:  {}", style(mem.name()).white().bold());
    println!(
        "  Health:   {}",
        if healthy {
            style("healthy").green().bold().to_string()
        } else {
            style("unhealthy").yellow().bold().to_string()
        }
    );
    println!("  Total:    {}", render_total(counted.as_ref()));

    let all = mem.list(None, None).await.unwrap_or_default();
    print!(
        "{}",
        render_category_breakdown(&all, counted.as_ref().ok().copied())
    );

    Ok(())
}

/// Render the `Total:` value.
///
/// This was `count().unwrap_or(0)`. `health_check` does not cover the gap —
/// sqlite's is `SELECT 1` and survives a damaged `memories` table, markdown's is
/// `workspace_dir.exists()` — so a store that could not be counted printed
/// `Total: 0` beside `Health: healthy` and read as an empty store.
fn render_total(counted: Result<&usize, &anyhow::Error>) -> String {
    match counted {
        Ok(total) => total.to_string(),
        Err(e) => format!("{} ({e:#})", style("unavailable").yellow().bold()),
    }
}

/// Render the per-category breakdown of `entries`.
///
/// `entries` is a **page**, not the whole store: `Memory::list` is capped by the
/// backend (`DEFAULT_LIST_LIMIT` on sqlite), so its length is a page size and
/// `count()` is the total. Building the breakdown from it without saying so
/// printed per-category numbers that silently summed to less than the `Total:`
/// line directly above them. `handle_list` and the TUI's `/memory list` already
/// carry this qualifier; this one was missed.
///
/// `total` is `count()`'s answer when it is available.
fn render_category_breakdown(
    entries: &[super::traits::MemoryEntry],
    total: Option<usize>,
) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for entry in entries {
        *counts.entry(entry.category.to_string()).or_default() += 1;
    }

    let listed = entries.len();
    let scope = match total {
        Some(total) if total > listed => format!(" (most recent {listed} of {total})"),
        _ => String::new(),
    };

    let mut out = format!("\n  By category{scope}:\n");
    let mut sorted: Vec<_> = counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    for (cat, count) in sorted {
        use std::fmt::Write as _;
        let _ = writeln!(out, "    {cat:<20} {count}");
    }
    out
}

async fn handle_clear(
    config: &Config,
    key: Option<String>,
    category: Option<String>,
    yes: bool,
) -> Result<()> {
    let mem = create_cli_memory(config)?;

    // Single-key deletion (exact or prefix match).
    if let Some(key) = key {
        let result = handle_clear_key(&*mem, &key, yes).await;
        // Deleting a core memory has to leave the projected block too.
        refresh_projection(&*mem, config);
        return result;
    }

    // Batch deletion by category (or all).
    let cat = category.as_deref().map(parse_category);
    let entries = mem.list(cat.as_ref(), None).await?;

    if entries.is_empty() {
        println!("No entries to clear.");
        return Ok(());
    }

    let scope = category.as_deref().unwrap_or("all categories");
    println!("Found {} entries in '{scope}'.", entries.len());

    if !yes {
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!("  Delete {} entries?", entries.len()))
            .default(false)
            .interact()?;
        if !confirmed {
            println!("Aborted.");
            return Ok(());
        }
    }

    let mut deleted = 0usize;
    for entry in &entries {
        if mem.forget(&entry.key).await? {
            deleted += 1;
        }
    }

    println!(
        "{} Cleared {deleted}/{} entries.",
        style("✓").green().bold(),
        entries.len(),
    );

    refresh_projection(&*mem, config);
    Ok(())
}

/// Delete a single entry by exact key or prefix match.
async fn handle_clear_key(mem: &dyn Memory, key: &str, yes: bool) -> Result<()> {
    // Resolve the target key (exact match or unique prefix).
    let target = if mem.get(key).await?.is_some() {
        key.to_string()
    } else {
        let all = mem.list(None, None).await?;
        let matches: Vec<_> = all.iter().filter(|e| e.key.starts_with(key)).collect();
        match matches.len() {
            0 => {
                println!("No memory entry found for key: {key}");
                return Ok(());
            }
            1 => matches[0].key.clone(),
            n => {
                println!("Prefix '{key}' matched {n} entries:\n");
                for entry in matches {
                    println!(
                        "- {} [{}]",
                        style(&entry.key).white().bold(),
                        entry.category
                    );
                }
                println!("\nSpecify a longer prefix to narrow the match.");
                return Ok(());
            }
        }
    };

    if !yes {
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!("  Delete '{target}'?"))
            .default(false)
            .interact()?;
        if !confirmed {
            println!("Aborted.");
            return Ok(());
        }
    }

    if mem.forget(&target).await? {
        println!("{} Deleted key: {target}", style("✓").green().bold());
    }

    Ok(())
}

fn parse_category(s: &str) -> MemoryCategory {
    match s.trim().to_ascii_lowercase().as_str() {
        "core" => MemoryCategory::Core,
        "daily" => MemoryCategory::Daily,
        "conversation" => MemoryCategory::Conversation,
        other => MemoryCategory::Custom(other.to_string()),
    }
}

fn truncate_content(s: &str, max_len: usize) -> String {
    let line = s.lines().next().unwrap_or(s);
    if line.len() <= max_len {
        return line.to_string();
    }
    let truncated: String = line.chars().take(max_len.saturating_sub(3)).collect();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `memory add` and `memory clear` used to leave `MEMORY.md` untouched: the
    /// CLI builds memory through a factory that skips embedding setup, and only
    /// the agent/gateway/TUI factory projected. A workspace seeded entirely from
    /// the command line therefore reached the model with an empty core tier.
    #[tokio::test]
    async fn cli_writes_refresh_the_memory_projection() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config::default();
        config.workspace_dir = tmp.path().to_path_buf();
        config.memory.backend = "sqlite".into();

        let projected = tmp.path().join(super::super::snapshot::MEMORY_FILE);

        handle_add(&config, "user_lang", "prefers Bahasa Indonesia", "core")
            .await
            .unwrap();
        let after_add = std::fs::read_to_string(&projected).expect("add must project");
        assert!(
            after_add.contains("- user_lang: prefers Bahasa Indonesia"),
            "{after_add}"
        );

        handle_clear(&config, Some("user_lang".into()), None, true)
            .await
            .unwrap();
        let after_clear = std::fs::read_to_string(&projected).expect("clear must project");
        assert!(
            !after_clear.contains("user_lang"),
            "a cleared memory must leave the block too:\n{after_clear}"
        );
    }

    /// The CLI borrowed the migration factory, so an operator running
    /// `memory stats` against a mistyped backend was told a migration was
    /// underway.
    #[test]
    fn cli_backend_errors_do_not_mention_migration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let error = create_memory_for_cli("sqlit", tmp.path())
            .err()
            .expect("an unknown backend is an error");
        let message = error.to_string();
        assert!(message.contains("sqlit"), "{message}");
        assert!(
            !message.contains("migration"),
            "the CLI is not migrating anything: {message}"
        );
    }

    #[test]
    fn migration_backend_errors_still_say_migration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let error = super::super::create_memory_for_migration("none", tmp.path())
            .err()
            .expect("backend 'none' is rejected");
        assert!(error.to_string().contains("migrate"), "{error}");
    }

    #[test]
    fn parse_category_known_variants() {
        assert_eq!(parse_category("core"), MemoryCategory::Core);
        assert_eq!(parse_category("daily"), MemoryCategory::Daily);
        assert_eq!(parse_category("conversation"), MemoryCategory::Conversation);
        assert_eq!(parse_category("CORE"), MemoryCategory::Core);
        assert_eq!(parse_category("  Daily  "), MemoryCategory::Daily);
    }

    #[test]
    fn parse_category_custom_fallback() {
        assert_eq!(
            parse_category("project_notes"),
            MemoryCategory::Custom("project_notes".into())
        );
    }

    #[test]
    fn truncate_content_short_text_unchanged() {
        assert_eq!(truncate_content("hello", 10), "hello");
    }

    #[test]
    fn truncate_content_long_text_truncated() {
        let result = truncate_content("this is a very long string", 10);
        assert!(result.ends_with("..."));
        assert!(result.chars().count() <= 10);
    }

    #[test]
    fn truncate_content_multiline_uses_first_line() {
        assert_eq!(truncate_content("first\nsecond", 20), "first");
    }

    #[test]
    fn truncate_content_empty_string() {
        assert_eq!(truncate_content("", 10), "");
    }

    // ── `memory stats` breakdown ──────────────────────────────────

    fn entries(categories: &[MemoryCategory]) -> Vec<super::super::traits::MemoryEntry> {
        categories
            .iter()
            .enumerate()
            .map(|(i, category)| super::super::traits::MemoryEntry {
                id: format!("id{i}"),
                key: format!("k{i}"),
                content: "filler".to_string(),
                category: category.clone(),
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                session_id: None,
                score: None,
            })
            .collect()
    }

    /// `Total:` comes from `count()`, the breakdown from `list()` — and `list()`
    /// is capped by the backend. Past the cap the two disagreed with nothing on
    /// screen saying the breakdown was partial.
    #[test]
    fn category_breakdown_is_labelled_when_the_page_is_shorter_than_the_total() {
        let page = entries(&[
            MemoryCategory::Core,
            MemoryCategory::Core,
            MemoryCategory::Core,
        ]);
        let out = render_category_breakdown(&page, Some(1100));

        assert!(
            out.contains("most recent 3 of 1100"),
            "partial breakdown not labelled: {out}"
        );
        assert!(out.contains("core"));
    }

    #[test]
    fn category_breakdown_is_unlabelled_when_the_page_is_the_whole_store() {
        let page = entries(&[MemoryCategory::Core, MemoryCategory::Daily]);
        let out = render_category_breakdown(&page, Some(2));

        assert!(
            out.contains("By category:"),
            "an exhaustive breakdown must not be qualified: {out}"
        );
        assert!(!out.contains("most recent"));
    }

    /// With `count()` unavailable there is no total to compare the page against,
    /// so the breakdown must not claim the page is partial either.
    #[test]
    fn category_breakdown_without_a_total_makes_no_claim() {
        let page = entries(&[MemoryCategory::Core]);
        let out = render_category_breakdown(&page, None);

        assert!(out.contains("By category:"));
        assert!(!out.contains("most recent"));
    }

    /// A store that cannot be counted must not be reported as an empty one.
    #[test]
    fn total_says_unavailable_rather_than_zero_when_count_fails() {
        let failure = anyhow::anyhow!("attempt to write a readonly database");
        let rendered = render_total(Err(&failure));

        assert!(
            rendered.contains("unavailable"),
            "a count failure rendered as: {rendered}"
        );
        assert!(
            rendered.contains("readonly database"),
            "the cause was dropped: {rendered}"
        );
        assert_ne!(rendered.trim(), "0");
    }

    #[test]
    fn total_renders_the_count_when_it_is_available() {
        assert_eq!(render_total(Ok(&1100)), "1100");
    }

    #[test]
    fn category_breakdown_of_an_empty_page_renders_nothing() {
        assert_eq!(render_category_breakdown(&[], Some(0)), "");
    }

    /// Equal counts used to fall out of a `HashMap` in whatever order it chose.
    #[test]
    fn category_breakdown_orders_ties_by_name() {
        let page = entries(&[MemoryCategory::Daily, MemoryCategory::Core]);
        let out = render_category_breakdown(&page, Some(2));

        let core = out.find("core").expect("core listed");
        let daily = out.find("daily").expect("daily listed");
        assert!(core < daily, "ties are not ordered by name: {out}");
    }
}
