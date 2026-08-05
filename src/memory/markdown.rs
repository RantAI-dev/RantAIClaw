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
        // Contract: the best hit in the returned set scores 1.0.
        super::vector::normalize_entry_scores(&mut scored);
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

    /// Remove the line holding `key`.
    ///
    /// This used to return `false` unconditionally, described as append-only for
    /// the audit trail. That reasoning does not survive the tool that calls it:
    /// `memory_forget` offers to "delete outdated facts or sensitive data", and
    /// answering "no memory found" about an entry that plainly exists is worse
    /// than either deleting it or refusing outright.
    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let mut removed = false;

        for path in self.all_memory_files().await? {
            let Ok(existing) = fs::read_to_string(&path).await else {
                continue;
            };

            let kept: Vec<&str> = existing
                .lines()
                .filter(|line| {
                    let clean = line.trim().strip_prefix("- ").unwrap_or(line.trim());
                    match split_stored_entry(clean) {
                        Some((stored_key, _)) if stored_key == key => {
                            removed = true;
                            false
                        }
                        _ => true,
                    }
                })
                .collect();

            if removed {
                let mut rewritten = kept.join("\n");
                if !rewritten.ends_with('\n') {
                    rewritten.push('\n');
                }
                fs::write(&path, rewritten).await?;
                break;
            }
        }

        Ok(removed)
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
}
