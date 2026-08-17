use super::traits::{Memory, MemoryCategory, MemoryEntry};
use async_trait::async_trait;
use chrono::Local;
use std::path::{Path, PathBuf};
use tokio::fs;

/// Split a stored line back into the key and content `store` wrote.
///
/// `store` renders `- **key**: content`; this is the inverse. Returns `None` for
/// a line an operator hand-wrote in some other shape, which then keeps its
/// positional identity rather than being mangled into one.
fn split_stored_entry(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix("**")?;
    let (key, content) = rest.split_once("**: ")?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), content.to_string()))
}

/// Markdown-based memory — plain files as source of truth
///
/// Layout:
///   workspace/MEMORY.md          — curated long-term memory (core)
///   workspace/memory/YYYY-MM-DD.md — daily logs (append-only)
pub struct MarkdownMemory {
    workspace_dir: PathBuf,
}

impl MarkdownMemory {
    pub fn new(workspace_dir: &Path) -> Self {
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
        }
    }

    fn memory_dir(&self) -> PathBuf {
        self.workspace_dir.join("memory")
    }

    fn core_path(&self) -> PathBuf {
        self.workspace_dir.join("MEMORY.md")
    }

    fn daily_path(&self) -> PathBuf {
        let date = Local::now().format("%Y-%m-%d").to_string();
        self.memory_dir().join(format!("{date}.md"))
    }

    async fn ensure_dirs(&self) -> anyhow::Result<()> {
        fs::create_dir_all(self.memory_dir()).await?;
        Ok(())
    }

    async fn append_to_file(&self, path: &Path, content: &str) -> anyhow::Result<()> {
        self.ensure_dirs().await?;

        let existing = if path.exists() {
            fs::read_to_string(path).await.unwrap_or_default()
        } else {
            String::new()
        };

        let updated = if existing.is_empty() {
            let header = if path == self.core_path() {
                "# Long-Term Memory\n\n"
            } else {
                let date = Local::now().format("%Y-%m-%d").to_string();
                &format!("# Daily Log — {date}\n\n")
            };
            format!("{header}{content}\n")
        } else {
            format!("{existing}\n{content}\n")
        };

        fs::write(path, updated).await?;
        Ok(())
    }

    fn parse_entries_from_file(
        path: &Path,
        content: &str,
        category: &MemoryCategory,
    ) -> Vec<MemoryEntry> {
        let filename = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");

        content
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                !trimmed.is_empty() && !trimmed.starts_with('#')
            })
            .enumerate()
            .map(|(i, line)| {
                let trimmed = line.trim();
                let clean = trimmed.strip_prefix("- ").unwrap_or(trimmed);
                // `store` writes `- **key**: content`, so read the key back out
                // of it. Keys used to be positional (`file:index`), which meant
                // they did not survive a round trip: `get`/`forget` could not
                // address an entry by the key it was stored under, and every
                // index shifted when a line was added above.
                let (key, content) = split_stored_entry(clean)
                    .unwrap_or_else(|| (format!("{filename}:{i}"), clean.to_string()));
                MemoryEntry {
                    id: format!("{filename}:{i}"),
                    key,
                    content,
                    category: category.clone(),
                    timestamp: filename.to_string(),
                    session_id: None,
                    score: None,
                }
            })
            .collect()
    }

    /// Drop every line in `path` stored under `key`. Returns whether any went.
    ///
    /// A line `split_stored_entry` cannot parse is not addressed by any key, so it
    /// is left alone — headers and the prose an operator hand-wrote in
    /// `MEMORY.md` are not ours to rewrite.
    ///
    /// Trailing blank lines are dropped from the result. Without that, the
    /// removal leaves the blank line that separated the entry behind, and
    /// `append_to_file` adds another on the next write — so re-storing one key
    /// repeatedly grew the file by a blank line each time. `MEMORY.md` is
    /// injected into the prompt, so that growth is a token cost.
    async fn remove_key_from_file(path: &Path, key: &str) -> anyhow::Result<bool> {
        let Ok(existing) = fs::read_to_string(path).await else {
            return Ok(false);
        };

        let mut removed = false;
        let mut kept: Vec<&str> = existing
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                let clean = trimmed.strip_prefix("- ").unwrap_or(trimmed);
                match split_stored_entry(clean) {
                    Some((stored_key, _)) if stored_key == key => {
                        removed = true;
                        false
                    }
                    _ => true,
                }
            })
            .collect();

        if !removed {
            return Ok(false);
        }

        while kept.last().is_some_and(|line| line.trim().is_empty()) {
            kept.pop();
        }

        let mut rewritten = kept.join("\n");
        if !rewritten.ends_with('\n') {
            rewritten.push('\n');
        }
        fs::write(path, rewritten).await?;

        Ok(true)
    }

    /// Every markdown file this backend owns: the core file, then the daily logs.
    async fn all_memory_files(&self) -> anyhow::Result<Vec<PathBuf>> {
        let mut paths = Vec::new();

        let core = self.core_path();
        if core.exists() {
            paths.push(core);
        }

        let mem_dir = self.memory_dir();
        if mem_dir.exists() {
            let mut dir = fs::read_dir(&mem_dir).await?;
            while let Some(entry) = dir.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    paths.push(path);
                }
            }
        }

        Ok(paths)
    }

    async fn read_all_entries(&self) -> anyhow::Result<Vec<MemoryEntry>> {
        let mut entries = Vec::new();

        // Read MEMORY.md (core)
        let core_path = self.core_path();
        if core_path.exists() {
            let content = fs::read_to_string(&core_path).await?;
            entries.extend(Self::parse_entries_from_file(
                &core_path,
                &content,
                &MemoryCategory::Core,
            ));
        }

        // Read daily logs
        let mem_dir = self.memory_dir();
        if mem_dir.exists() {
            let mut dir = fs::read_dir(&mem_dir).await?;
            while let Some(entry) = dir.next_entry().await? {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("md") {
                    let content = fs::read_to_string(&path).await?;
                    entries.extend(Self::parse_entries_from_file(
                        &path,
                        &content,
                        &MemoryCategory::Daily,
                    ));
                }
            }
        }

        entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        Ok(entries)
    }
}

#[async_trait]
impl Memory for MarkdownMemory {
    fn name(&self) -> &str {
        "markdown"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
        _session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // Replace, don't append. `SqliteMemory` upserts on `key`
        // (`ON CONFLICT(key) DO UPDATE`) and `PostgresMemory` matches it; one
        // `Memory` trait has to mean one thing. Appending left a second line under
        // the same key, which inflated `count()` — rendered as `Total:` by
        // `memory stats` — showed the key twice in `list()`, and left `get()`
        // returning whichever copy sorted first under a `timestamp` that is really
        // the filename.
        //
        // `forget` clears the key from *every* file, not just the one about to be
        // written, so a re-store under a different category moves the entry rather
        // than duplicating it across two files. That is what sqlite's upsert does
        // to `category`.
        self.forget(key).await?;

        let entry = format!("- **{key}**: {content}");
        let path = match category {
            MemoryCategory::Core => self.core_path(),
            _ => self.daily_path(),
        };
        self.append_to_file(&path, &entry).await
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let all = self.read_all_entries().await?;
        let query_lower = query.to_lowercase();
        let keywords: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<MemoryEntry> = all
            .into_iter()
            .filter_map(|mut entry| {
                let content_lower = entry.content.to_lowercase();
                let matched = keywords
                    .iter()
                    .filter(|kw| content_lower.contains(**kw))
                    .count();
                if matched > 0 {
                    #[allow(clippy::cast_precision_loss)]
                    let score = matched as f64 / keywords.len() as f64;
                    entry.score = Some(score);
                    Some(entry)
                } else {
                    None
                }
            })
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Query coverage is already an absolute [0, 1] relevance — no
        // best-hit rescale, so a set of weak hits stays under the floor.
        scored.truncate(limit);
        Ok(scored)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let all = self.read_all_entries().await?;
        Ok(all
            .into_iter()
            .find(|e| e.key == key || e.content.contains(key)))
    }

    async fn list(
        &self,
        category: Option<&MemoryCategory>,
        _session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let all = self.read_all_entries().await?;
        match category {
            Some(cat) => Ok(all.into_iter().filter(|e| &e.category == cat).collect()),
            None => Ok(all),
        }
    }

    /// Remove the line holding `key`, from **every** file this backend owns.
    ///
    /// This used to return `false` unconditionally, described as append-only for
    /// the audit trail. That reasoning does not survive the tool that calls it:
    /// `memory_forget` offers to "delete outdated facts or sensitive data", and
    /// answering "no memory found" about an entry that plainly exists is worse
    /// than either deleting it or refusing outright.
    ///
    /// It then stopped at the first file that matched, which reintroduced the
    /// same wrong answer from the other direction: a key present in `MEMORY.md`
    /// *and* a daily log lost one copy, kept the rest, and still reported `true`.
    /// Sweep all of them.
    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let mut removed_any = false;

        for path in self.all_memory_files().await? {
            if Self::remove_key_from_file(&path, key).await? {
                removed_any = true;
            }
        }

        Ok(removed_any)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let all = self.read_all_entries().await?;
        Ok(all.len())
    }

    async fn health_check(&self) -> bool {
        self.workspace_dir.exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_workspace() -> (TempDir, MarkdownMemory) {
        let tmp = TempDir::new().unwrap();
        let mem = MarkdownMemory::new(tmp.path());
        (tmp, mem)
    }

    #[tokio::test]
    async fn markdown_name() {
        let (_tmp, mem) = temp_workspace();
        assert_eq!(mem.name(), "markdown");
    }

    #[tokio::test]
    async fn markdown_health_check() {
        let (_tmp, mem) = temp_workspace();
        assert!(mem.health_check().await);
    }

    #[tokio::test]
    async fn markdown_store_core() {
        let (_tmp, mem) = temp_workspace();
        mem.store("pref", "User likes Rust", MemoryCategory::Core, None)
            .await
            .unwrap();
        let content = fs::read_to_string(mem.core_path()).await.unwrap();
        assert!(content.contains("User likes Rust"));
    }

    #[tokio::test]
    async fn markdown_store_daily() {
        let (_tmp, mem) = temp_workspace();
        mem.store("note", "Finished tests", MemoryCategory::Daily, None)
            .await
            .unwrap();
        let path = mem.daily_path();
        let content = fs::read_to_string(path).await.unwrap();
        assert!(content.contains("Finished tests"));
    }

    #[tokio::test]
    async fn markdown_recall_keyword() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "Rust is fast", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "Python is slow", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("c", "Rust and safety", MemoryCategory::Core, None)
            .await
            .unwrap();

        let results = mem.recall("Rust", 10, None).await.unwrap();
        assert!(results.len() >= 2);
        assert!(results
            .iter()
            .all(|r| r.content.to_lowercase().contains("rust")));
    }

    #[tokio::test]
    async fn markdown_recall_no_match() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "Rust is great", MemoryCategory::Core, None)
            .await
            .unwrap();
        let results = mem.recall("javascript", 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn markdown_count() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "first", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "second", MemoryCategory::Core, None)
            .await
            .unwrap();
        let count = mem.count().await.unwrap();
        assert!(count >= 2);
    }

    #[tokio::test]
    async fn markdown_list_by_category() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "core fact", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "daily note", MemoryCategory::Daily, None)
            .await
            .unwrap();

        let core = mem.list(Some(&MemoryCategory::Core), None).await.unwrap();
        assert!(core.iter().all(|e| e.category == MemoryCategory::Core));

        let daily = mem.list(Some(&MemoryCategory::Daily), None).await.unwrap();
        assert!(daily.iter().all(|e| e.category == MemoryCategory::Daily));
    }

    #[tokio::test]
    /// `forget` used to return `false` unconditionally, so `memory_forget` —
    /// which offers to delete sensitive data — reported "no memory found" about
    /// an entry that plainly existed.
    async fn markdown_forget_removes_the_entry() {
        let (_tmp, mem) = temp_workspace();
        mem.store("doomed", "delete me", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("keeper", "keep me", MemoryCategory::Core, None)
            .await
            .unwrap();

        let removed = mem.forget("doomed").await.unwrap();
        assert!(removed, "an entry that exists must be reported as removed");

        let remaining = mem.list(None, None).await.unwrap();
        let keys: Vec<&str> = remaining.iter().map(|e| e.key.as_str()).collect();
        assert!(!keys.contains(&"doomed"), "still present: {keys:?}");
        assert!(
            keys.contains(&"keeper"),
            "took a neighbour with it: {keys:?}"
        );
    }

    #[tokio::test]
    async fn markdown_forget_reports_false_for_a_missing_key() {
        let (_tmp, mem) = temp_workspace();
        assert!(!mem.forget("never_stored").await.unwrap());
    }

    /// `store` writes `- **key**: content`; keys used to be read back
    /// positionally, so they did not survive the round trip and every index
    /// shifted when a line was added above.
    #[tokio::test]
    async fn markdown_keys_round_trip_through_storage() {
        let (_tmp, mem) = temp_workspace();
        mem.store("user_lang", "prefers Rust", MemoryCategory::Core, None)
            .await
            .unwrap();

        let fetched = mem.get("user_lang").await.unwrap();
        assert!(fetched.is_some(), "an entry must be addressable by its key");
        assert_eq!(fetched.unwrap().content, "prefers Rust");
    }

    #[tokio::test]
    async fn markdown_empty_recall() {
        let (_tmp, mem) = temp_workspace();
        let results = mem.recall("anything", 10, None).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn markdown_empty_count() {
        let (_tmp, mem) = temp_workspace();
        assert_eq!(mem.count().await.unwrap(), 0);
    }

    /// Contract: the best hit in a returned set scores 1.0 and nothing exceeds
    /// it, so one relevance threshold means the same thing on every backend.
    #[tokio::test]
    async fn markdown_recall_normalises_scores_to_best() {
        let (_tmp, mem) = temp_workspace();
        mem.store("a", "rust ownership model", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("b", "rust only", MemoryCategory::Core, None)
            .await
            .unwrap();

        let hits = mem.recall("rust ownership", 10, None).await.unwrap();
        assert!(!hits.is_empty());

        let best = hits.iter().filter_map(|e| e.score).fold(0.0_f64, f64::max);
        assert!(
            (best - 1.0).abs() < 1e-6,
            "best hit must score 1.0, got {best}"
        );
        for e in &hits {
            let s = e.score.unwrap();
            assert!((0.0..=1.0).contains(&s), "{} out of range: {s}", e.key);
        }
    }

    // ── store replaces; forget sweeps every file ───────────────────

    /// `store` appended, so one key became two lines: `count()` inflated,
    /// `list()` showed it twice, and `get()` returned whichever copy sorted first.
    #[tokio::test]
    async fn markdown_store_replaces_an_existing_key() {
        let (_tmp, mem) = temp_workspace();
        mem.store("k", "old value", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("k", "new value", MemoryCategory::Core, None)
            .await
            .unwrap();

        assert_eq!(mem.count().await.unwrap(), 1, "one key counts once");
        assert_eq!(mem.get("k").await.unwrap().unwrap().content, "new value");

        let rows: Vec<_> = mem
            .list(None, None)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.key == "k")
            .collect();
        assert_eq!(rows.len(), 1, "stale row survives: {rows:?}");
    }

    /// A re-store under a different category moves the entry, matching what
    /// sqlite's `ON CONFLICT(key) DO UPDATE SET category = excluded.category` does.
    #[tokio::test]
    async fn markdown_store_moves_an_entry_across_categories() {
        let (_tmp, mem) = temp_workspace();
        mem.store("k", "core copy", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("k", "daily copy", MemoryCategory::Daily, None)
            .await
            .unwrap();

        assert_eq!(mem.count().await.unwrap(), 1);
        let entry = mem.get("k").await.unwrap().unwrap();
        assert_eq!(entry.content, "daily copy");
        assert_eq!(entry.category, MemoryCategory::Daily);

        let core = fs::read_to_string(mem.core_path()).await.unwrap();
        assert!(
            !core.contains("core copy"),
            "the old file still holds it:\n{core}"
        );
    }

    /// `forget` stopped at the first file that matched, so a key present in both
    /// `MEMORY.md` and a daily log lost one copy, kept the rest, and reported
    /// `true` — which `memory_forget` renders as "Forgot memory: k".
    #[tokio::test]
    async fn markdown_forget_sweeps_every_file() {
        let (_tmp, mem) = temp_workspace();
        // Write the duplicate directly: `store` no longer produces this state, but
        // a workspace written by an earlier build can still be in it.
        mem.append_to_file(&mem.core_path(), "- **dupe**: core copy")
            .await
            .unwrap();
        mem.append_to_file(&mem.daily_path(), "- **dupe**: daily copy")
            .await
            .unwrap();
        assert_eq!(mem.count().await.unwrap(), 2, "control: both copies exist");

        assert!(mem.forget("dupe").await.unwrap());
        assert!(
            mem.get("dupe").await.unwrap().is_none(),
            "forget reported true but an entry survives"
        );
        assert_eq!(mem.count().await.unwrap(), 0);
    }

    /// `MEMORY.md` is a file operators edit. A line that is not a stored entry is
    /// not addressed by any key and must survive both operations.
    #[tokio::test]
    async fn markdown_hand_written_lines_survive_store_and_forget() {
        let (_tmp, mem) = temp_workspace();
        mem.store("k", "stored value", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.append_to_file(&mem.core_path(), "Prose an operator wrote.")
            .await
            .unwrap();

        mem.store("other", "another value", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.forget("k").await.unwrap();

        let core = fs::read_to_string(mem.core_path()).await.unwrap();
        assert!(
            core.contains("Prose an operator wrote."),
            "hand-written line was rewritten away:\n{core}"
        );
        assert!(core.contains("# Long-Term Memory"), "header lost:\n{core}");
    }

    #[tokio::test]
    async fn markdown_forget_absent_key_rewrites_nothing() {
        let (_tmp, mem) = temp_workspace();
        mem.store("k", "value", MemoryCategory::Core, None)
            .await
            .unwrap();

        let path = mem.core_path();
        let before = fs::read_to_string(&path).await.unwrap();
        let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();

        assert!(!mem.forget("absent").await.unwrap());

        let after = fs::read_to_string(&path).await.unwrap();
        let after_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after, "contents changed");
        assert_eq!(before_mtime, after_mtime, "file was rewritten");
    }

    /// Re-storing one key repeatedly must not grow the file. The removal leaves
    /// the blank line that separated the entry, and `append_to_file` adds another
    /// on the next write — unbounded growth in a file the prompt injects.
    #[tokio::test]
    async fn markdown_repeated_store_does_not_grow_the_file() {
        let (_tmp, mem) = temp_workspace();
        mem.store("k", "v1", MemoryCategory::Core, None)
            .await
            .unwrap();
        let after_first = fs::read_to_string(mem.core_path()).await.unwrap();

        for i in 2..6 {
            mem.store("k", &format!("v{i}"), MemoryCategory::Core, None)
                .await
                .unwrap();
        }
        let after_many = fs::read_to_string(mem.core_path()).await.unwrap();

        assert_eq!(
            after_first.lines().count(),
            after_many.lines().count(),
            "file grew across re-stores:\n{after_many}"
        );
        assert!(after_many.contains("- **k**: v5"));
    }
}
