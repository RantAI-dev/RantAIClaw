//! Memory embedding configuration check.
//!
//! Validates the resolved embedding provider name against what the runtime
//! factory understands, names the live search mode, and warns when rows in
//! `brain.db` were embedded by a different model.

use async_trait::async_trait;
use rusqlite::{Connection, OpenFlags};

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext};

pub struct MemorySearchModeCheck;

#[async_trait]
impl DoctorCheck for MemorySearchModeCheck {
    fn name(&self) -> &'static str {
        "memory.embedding"
    }
    fn category(&self) -> &'static str {
        "config"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let cat = self.category();
        let resolved = crate::memory::resolve_embedding_config(
            &ctx.config.memory,
            &ctx.config.embedding_routes,
            ctx.config.api_key.as_deref(),
        );
        let provider = resolved.provider.trim();
        let mode = crate::memory::search_mode_label(provider);

        if provider.is_empty() || provider.eq_ignore_ascii_case("none") {
            return CheckResult::ok(self.name(), format!("memory search mode: {mode}"))
                .with_category(cat);
        }

        if !is_known_embedding_provider(provider) {
            return CheckResult::fail(
                self.name(),
                format!(
                    "unknown embedding provider '{provider}'; \
                     expected one of: none, openai, openrouter, minimax, custom:<base-url>"
                ),
            )
            .with_category(cat)
            .with_hint("run: rantaiclaw setup provider");
        }

        match foreign_embedded_rows(&ctx.config.workspace_dir, provider, resolved.dimensions) {
            Ok(0) => CheckResult::ok(self.name(), format!("memory search mode: {mode}"))
                .with_category(cat),
            Ok(n) => CheckResult::warn(
                self.name(),
                format!("{n} row(s) embedded by a different model ({mode} is now live)"),
            )
            .with_category(cat)
            .with_hint("run: rantaiclaw memory reindex"),
            Err(()) => CheckResult::ok(
                self.name(),
                format!("memory search mode: {mode} (no memory store yet)"),
            )
            .with_category(cat),
        }
    }
}

/// Mirrors the match in [`crate::memory::embeddings::create_embedding_provider`]:
/// `openai` / `openrouter` / `minimax` and the `custom:<base-url>` prefix are
/// known, anything else is a typo that the factory silently downgrades to
/// `NoopEmbedding`. Centralised here so the doctor and the factory cannot
/// disagree on what "known" means.
fn is_known_embedding_provider(provider: &str) -> bool {
    matches!(provider, "openai" | "openrouter" | "minimax") || provider.starts_with("custom:")
}

/// Open the workspace's `brain.db` read-only and count rows whose stored
/// embedding was produced by a different model than the live one.
///
/// `Ok(n)` — db is openable, n rows need re-embedding (0 means all rows
///           match the live model).
/// `Err(())` — db missing or unopenable; treated as "no memory store yet" so
///             a fresh install does not fail the doctor.
fn foreign_embedded_rows(
    workspace_dir: &std::path::Path,
    live_provider: &str,
    live_dims: usize,
) -> Result<i64, ()> {
    let db_path = workspace_dir.join("memory").join("brain.db");
    let conn =
        Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|_| ())?;
    conn.query_row(
        "SELECT COUNT(*) FROM memories \
         WHERE embedding IS NOT NULL \
           AND (embedding_model IS NULL \
                OR embedding_dims IS NULL \
                OR embedding_model != ?1 \
                OR embedding_dims != ?2)",
        rusqlite::params![live_provider, live_dims as i64],
        |row| row.get(0),
    )
    .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::doctor::Severity;
    use crate::profile::Profile;
    use tempfile::TempDir;

    fn ctx_with_workspace(cfg: Config, workspace: &std::path::Path) -> DoctorContext {
        DoctorContext {
            profile: Profile {
                name: "test".into(),
                root: workspace.to_path_buf(),
            },
            config: cfg,
            offline: false,
        }
    }

    /// The default `embedding_provider = "none"` is the configured-absent
    /// state — keyword mode is what runs, and the check names that.
    #[tokio::test]
    async fn check_passes_for_the_default_none_provider() {
        let tmp = TempDir::new().unwrap();
        let cfg = Config::default();
        assert_eq!(
            cfg.memory.embedding_provider, "none",
            "precondition: default is none"
        );
        let ctx = ctx_with_workspace(cfg, tmp.path());
        let result = MemorySearchModeCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
        assert!(
            result.message.contains("keyword"),
            "the message must name the live mode: {}",
            result.message
        );
        assert_eq!(result.category, "config");
    }

    /// An empty-string provider name is the configured-absent form of `none`.
    /// No foreign rows to look up against, and the message names keyword mode.
    #[tokio::test]
    async fn check_passes_for_an_empty_provider_name() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = Config::default();
        cfg.memory.embedding_provider = String::new();
        let ctx = ctx_with_workspace(cfg, tmp.path());
        let result = MemorySearchModeCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
        assert!(result.message.contains("keyword"), "{}", result.message);
    }

    /// An unknown provider name — the silent downgrade the factory used to
    /// ship — is now a doctor Fail. The daemon still starts (that's the
    /// factory's contract), so the doctor is what surfaces the typo.
    /// The message lists the valid values so an operator can pick one.
    #[tokio::test]
    async fn check_fails_for_an_unknown_provider() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = Config::default();
        cfg.memory.embedding_provider = "cohere".into();
        let ctx = ctx_with_workspace(cfg, tmp.path());
        let result = MemorySearchModeCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail, "msg: {}", result.message);
        assert!(result.message.contains("cohere"), "{}", result.message);
        assert!(
            result.message.contains("none"),
            "the message must list `none` as a valid value: {}",
            result.message
        );
        assert!(
            result.message.contains("custom:"),
            "the message must list the custom prefix: {}",
            result.message
        );
        assert!(result.hint.is_some(), "the fail must hint at a remedy");
    }

    /// A known provider with a missing `brain.db` (fresh install) must not
    /// fail the doctor — there is no store to be inconsistent with yet.
    #[tokio::test]
    async fn check_passes_when_known_provider_has_no_brain_db_yet() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = Config::default();
        cfg.memory.embedding_provider = "openai".into();
        cfg.memory.embedding_dimensions = 1536;
        let ctx = ctx_with_workspace(cfg, tmp.path());
        let result = MemorySearchModeCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
        assert!(
            result.message.contains("keyword + semantic"),
            "the message must name the hybrid mode: {}",
            result.message
        );
    }

    /// Rows embedded by a previous model are visible to vector search as
    /// zeros and silently emptied it. The check warns and points at
    /// `rantaiclaw memory reindex`.
    #[tokio::test]
    async fn check_warns_when_rows_were_embedded_by_a_foreign_model() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&db_dir).unwrap();
        let conn = Connection::open(db_dir.join("brain.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY,
                key TEXT NOT NULL UNIQUE,
                content TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'core',
                embedding BLOB,
                embedding_model TEXT,
                embedding_dims INTEGER,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            INSERT INTO memories (id, key, content, embedding, embedding_model, embedding_dims,
                                  created_at, updated_at)
            VALUES ('1', 'old', 'old fact', X'deadbeef', 'openai', 1536,
                    '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');",
        )
        .unwrap();
        drop(conn);

        let mut cfg = Config::default();
        cfg.workspace_dir = tmp.path().to_path_buf();
        cfg.memory.embedding_provider = "openai".into();
        cfg.memory.embedding_model = "text-embedding-3-large".into();
        cfg.memory.embedding_dimensions = 3072;
        let ctx = ctx_with_workspace(cfg, tmp.path());
        let result = MemorySearchModeCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
        let hint = result.hint.as_deref().unwrap_or_default();
        assert!(
            hint.contains("reindex"),
            "hint must point at reindex: {hint}"
        );
    }

    /// Rows whose embedding identity matches the live config do not warn.
    #[tokio::test]
    async fn check_passes_when_stored_rows_match_the_live_model() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&db_dir).unwrap();
        let conn = Connection::open(db_dir.join("brain.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE memories (
                id TEXT PRIMARY KEY,
                key TEXT NOT NULL UNIQUE,
                content TEXT NOT NULL,
                category TEXT NOT NULL DEFAULT 'core',
                embedding BLOB,
                embedding_model TEXT,
                embedding_dims INTEGER,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            INSERT INTO memories (id, key, content, embedding, embedding_model, embedding_dims,
                                  created_at, updated_at)
            VALUES ('1', 'k', 'fact', X'deadbeef', 'openai', 1536,
                    '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');",
        )
        .unwrap();
        drop(conn);

        let mut cfg = Config::default();
        cfg.workspace_dir = tmp.path().to_path_buf();
        cfg.memory.embedding_provider = "openai".into();
        cfg.memory.embedding_dimensions = 1536;
        let ctx = ctx_with_workspace(cfg, tmp.path());
        let result = MemorySearchModeCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
        assert!(
            result.message.contains("keyword + semantic"),
            "{}",
            result.message
        );
    }

    /// A `hint:` route that names a known provider flows through
    /// `resolve_embedding_config`. The check uses the routed provider, not
    /// the raw `[memory].embedding_provider` value.
    #[tokio::test]
    async fn check_honours_the_routed_provider() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = Config::default();
        cfg.memory.embedding_provider = "none".into();
        cfg.memory.embedding_model = "hint:semantic".into();
        cfg.embedding_routes = vec![crate::config::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "custom:https://api.example.com/v1".into(),
            model: "custom-embed".into(),
            dimensions: Some(1024),
            api_key: None,
        }];
        let ctx = ctx_with_workspace(cfg, tmp.path());
        let result = MemorySearchModeCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
        assert!(
            result.message.contains("custom:https://api.example.com/v1"),
            "the routed provider must appear in the message: {}",
            result.message
        );
    }

    #[test]
    fn is_known_embedding_provider_matches_the_factory() {
        assert!(is_known_embedding_provider("openai"));
        assert!(is_known_embedding_provider("openrouter"));
        assert!(is_known_embedding_provider("minimax"));
        assert!(is_known_embedding_provider("custom:http://localhost:1234"));
        assert!(!is_known_embedding_provider("cohere"));
        assert!(!is_known_embedding_provider("none"));
        assert!(!is_known_embedding_provider(""));
    }
}
