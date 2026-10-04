pub mod backend;
pub mod chunker;
pub mod cli;
pub mod context;
pub mod embeddings;
pub mod hygiene;
pub mod none;
pub mod sanitize;
pub mod saved;
pub mod snapshot;
pub mod sqlite;
pub(crate) mod terms;
pub mod traits;
pub mod vector;
pub mod view;

#[allow(unused_imports)]
pub use backend::{
    classify_memory_backend, default_memory_backend_key, memory_backend_profile,
    selectable_memory_backends, MemoryBackendKind, MemoryBackendProfile,
};
pub use context::{
    build_memory_context, build_memory_context_in_view, prepend_memory_block, MemoryContext,
    MemoryContextLimits,
};
pub use none::NoneMemory;
pub use sanitize::sanitize_memory_content;
pub use saved::{record_saved_note, SavedNotes, SAVED_NOTES};
pub use sqlite::SqliteMemory;
pub use traits::{KeyInUse, Memory};
#[allow(unused_imports)]
pub use traits::{MemoryCategory, MemoryEntry};
pub use view::{
    current_memory_view, forget_in_view, recall_in_view, store_in_view, MemoryView,
    DELETED_NOTE_HELD_BY_HISTORY, MEMORY_VIEW, NO_MEMORY_VIEW_REFUSAL,
};

use crate::config::{EmbeddingRouteConfig, MemoryConfig};
use std::path::Path;
use std::sync::Arc;

/// The `MEMORY.md` scaffold the onboarding wizard writes for a new workspace.
///
/// Shared with the markdown → sqlite importer, which skips a line only when
/// it matches one of these exactly (after trim): a prefix match let real
/// operator lines that happened to start with the same words get dropped as
/// scaffold, and missed lines that used no bullet prefix at all.
pub(crate) const MEMORY_MD_TEMPLATE: &str = "\
    # MEMORY.md — Long-Term Memory\n\n\
    *Your curated memories. The distilled essence, not raw logs.*\n\n\
    ## How This Works\n\
    - This file captures what's WORTH KEEPING long-term\n\
    - This file is auto-injected into your system prompt each session\n\
    - Every character here costs tokens\n\n\
    ## Security\n\
    - ONLY loaded in main session (direct chat with your human)\n\
    - NEVER loaded in group chats or shared contexts\n\n\
    ---\n\n\
    ## Key Facts\n\
    (None yet)\n\n\
    ## Decisions & Preferences\n\
    (None yet)\n\n\
    ## Lessons Learned\n\
    (None yet)\n\n\
    ## Open Loops\n\
    (None yet)\n";

/// Lines earlier wizards wrote into `MEMORY.md` that [`MEMORY_MD_TEMPLATE`] no
/// longer holds: the line about daily files that no longer exist, the four
/// placeholders that told the model to write into the file, and the bullet that
/// told it to keep the file short. A `MEMORY.md` written then still holds them,
/// so the importer keeps treating them as scaffold, not as notes. Compared after
/// trimming and without a leading `- `, like the template lines.
pub(crate) const LEGACY_MEMORY_MD_SCAFFOLD_LINES: &[&str] = &[
    "Daily files (`memory/YYYY-MM-DD.md`) capture raw events (on-demand via tools)",
    "Keep it concise — every character here costs tokens",
    "(Add important facts about your human here)",
    "(Record decisions and preferences here)",
    "(Document mistakes and insights here)",
    "(Track unfinished tasks and follow-ups here)",
];

fn create_memory_with_builders<F>(
    backend_name: &str,
    mut sqlite_builder: F,
    unknown_context: &str,
) -> anyhow::Result<Box<dyn Memory>>
where
    F: FnMut() -> anyhow::Result<SqliteMemory>,
{
    match classify_memory_backend(backend_name) {
        MemoryBackendKind::Sqlite => Ok(Box::new(sqlite_builder()?)),
        MemoryBackendKind::None => Ok(Box::new(NoneMemory::new())),
        MemoryBackendKind::Unknown => {
            // An unrecognised `backend` value is a configuration error. Falling
            // back to a default backend used to silently run a different store
            // with different semantics — for example a `forget` that did
            // nothing — while a warning scrolled past.
            anyhow::bail!(
                "unknown memory backend '{backend_name}'{unknown_context}; \
                 expected one of: sqlite, none"
            )
        }
    }
}

pub fn effective_memory_backend_name(memory_backend: &str) -> String {
    let resolved = memory_backend.trim().to_ascii_lowercase();

    if matches!(resolved.as_str(), "lucid" | "markdown" | "postgres") {
        warn_retired_backend_once();
        return "sqlite".to_string();
    }

    resolved
}

/// One-time WARN that the memory backend has been retired.
///
/// `lucid`, `markdown`, and `postgres` were all retired in favour of
/// `sqlite`. A config this binary never touched before would
/// otherwise stop the daemon with no actionable error. The WARN is one-shot
/// to keep the log from filling with the same line every turn.
static RETIRED_BACKEND_WARN: std::sync::OnceLock<()> = std::sync::OnceLock::new();

fn warn_retired_backend_once() {
    if RETIRED_BACKEND_WARN.set(()).is_ok() {
        tracing::warn!(
            "memory backends 'lucid', 'markdown' and 'postgres' were retired; use sqlite. \
             Resolving to 'sqlite'. Set memory.backend = \"sqlite\" in config.toml \
             to silence this message."
        );
    }
}

/// Build a per-turn auto-save key.
///
/// `memories.key` is UNIQUE and `store` upserts on conflict, so a fixed key makes
/// each turn overwrite the last. Every auto-save write site must go through this
/// True for a key this runtime generated rather than a person naming a fact.
///
/// Auto-save writes one entry per turn under `<prefix>_<uuid>`. The uuid is an
/// address, not a name: showing it to an operator identifies nothing, so
/// surfaces that list recalled memories summarise these instead of naming them.
pub fn is_autosave_key(key: &str) -> bool {
    let normalized = key.trim().to_ascii_lowercase();
    let Some((_, suffix)) = normalized.rsplit_once('_') else {
        return false;
    };
    // A v4 uuid tail is what auto-save appends; anything else is a chosen name.
    suffix.len() == 36
        && suffix.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        && suffix.matches('-').count() == 4
}

/// Legacy auto-save key used for model-authored assistant summaries.
/// These entries are treated as untrusted context and should not be re-injected.
pub fn is_assistant_autosave_key(key: &str) -> bool {
    let normalized = key.trim().to_ascii_lowercase();
    normalized == "assistant_resp" || normalized.starts_with("assistant_resp_")
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ResolvedEmbeddingConfig {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) dimensions: usize,
    pub(crate) api_key: Option<String>,
}

impl std::fmt::Debug for ResolvedEmbeddingConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedEmbeddingConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

pub(crate) fn resolve_embedding_config(
    config: &MemoryConfig,
    embedding_routes: &[EmbeddingRouteConfig],
    api_key: Option<&str>,
) -> ResolvedEmbeddingConfig {
    let fallback_api_key = api_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let fallback = ResolvedEmbeddingConfig {
        provider: config.embedding_provider.trim().to_string(),
        model: config.embedding_model.trim().to_string(),
        dimensions: config.embedding_dimensions,
        api_key: fallback_api_key.clone(),
    };

    let Some(hint) = config
        .embedding_model
        .strip_prefix("hint:")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return fallback;
    };

    let Some(route) = embedding_routes
        .iter()
        .find(|route| route.hint.trim() == hint)
    else {
        tracing::warn!(
            hint,
            "Unknown embedding route hint; falling back to [memory] embedding settings"
        );
        return fallback;
    };

    let provider = route.provider.trim();
    let model = route.model.trim();
    let dimensions = route.dimensions.unwrap_or(config.embedding_dimensions);
    if provider.is_empty() || model.is_empty() || dimensions == 0 {
        tracing::warn!(
            hint,
            "Invalid embedding route configuration; falling back to [memory] embedding settings"
        );
        return fallback;
    }

    let routed_api_key = route
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|value: &&str| !value.is_empty())
        .map(|value| value.to_string());

    ResolvedEmbeddingConfig {
        provider: provider.to_string(),
        model: model.to_string(),
        dimensions,
        api_key: routed_api_key.or(fallback_api_key),
    }
}

/// Render the search mode for the surfaces that report on memory: the
/// `memory stats` CLI, the `memory recall` CLI header, and the
/// `GET /api/v1/memory/stats` `mode` field.
///
/// Kept as one fn so the three surfaces cannot drift. The factory
/// (`create_embedding_provider`) and `SqliteMemory::recall` share the same
/// effective mode: a `none` provider runs FTS-only (keyword) and any other
/// known provider runs hybrid (keyword + semantic).
pub fn search_mode_label(provider: &str) -> String {
    let trimmed = provider.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return "keyword".to_string();
    }
    format!("keyword + semantic ({trimmed})")
}

/// Factory: create the right memory backend from config
pub fn create_memory(
    config: &MemoryConfig,
    workspace_dir: &Path,
    api_key: Option<&str>,
) -> anyhow::Result<Box<dyn Memory>> {
    create_memory_with_storage_and_routes(config, &[], workspace_dir, api_key)
}

/// Factory: create memory with no embedding routes.
///
/// The `[storage]` section used to carry a per-config backend override
/// (`storage.provider.config.provider`); that section was retired together
/// with `postgres`, so the override is gone and the backend name now comes
/// from `memory.backend` only. Kept as a thin wrapper so call sites that
/// previously used the override path do not have to switch to `create_memory`.
pub fn create_memory_with_storage(
    config: &MemoryConfig,
    workspace_dir: &Path,
    api_key: Option<&str>,
) -> anyhow::Result<Box<dyn Memory>> {
    create_memory_with_storage_and_routes(config, &[], workspace_dir, api_key)
}

/// Factory: create memory with embedding routes.
pub fn create_memory_with_storage_and_routes(
    config: &MemoryConfig,
    embedding_routes: &[EmbeddingRouteConfig],
    workspace_dir: &Path,
    api_key: Option<&str>,
) -> anyhow::Result<Box<dyn Memory>> {
    let backend_name = effective_memory_backend_name(&config.backend);
    let backend_kind = classify_memory_backend(&backend_name);
    let resolved_embedding = resolve_embedding_config(config, embedding_routes, api_key);

    // Best-effort memory hygiene/retention pass (throttled by state file).
    if let Err(e) = hygiene::run_if_due(config, workspace_dir) {
        tracing::warn!("memory hygiene skipped: {e}");
    }

    // Auto-hydration: if brain.db is missing but MEMORY_SNAPSHOT.md exists,
    // restore the "soul" from the snapshot before creating the backend.
    if config.auto_hydrate
        && matches!(backend_kind, MemoryBackendKind::Sqlite)
        && snapshot::should_hydrate(workspace_dir)
    {
        tracing::info!("🧬 Cold boot detected — hydrating from MEMORY_SNAPSHOT.md");
        match snapshot::hydrate_from_snapshot(workspace_dir) {
            Ok(count) => {
                if count > 0 {
                    tracing::info!("🧬 Hydrated {count} core memories from snapshot");
                }
            }
            Err(e) => {
                tracing::warn!("memory hydration failed: {e}");
            }
        }
    }

    fn build_sqlite_memory(
        config: &MemoryConfig,
        workspace_dir: &Path,
        resolved_embedding: &ResolvedEmbeddingConfig,
    ) -> anyhow::Result<SqliteMemory> {
        let embedder: Arc<dyn embeddings::EmbeddingProvider> =
            Arc::from(embeddings::create_embedding_provider(
                &resolved_embedding.provider,
                resolved_embedding.api_key.as_deref(),
                &resolved_embedding.model,
                resolved_embedding.dimensions,
            ));

        #[allow(clippy::cast_possible_truncation)]
        let mem = SqliteMemory::with_embedder(
            workspace_dir,
            embedder,
            config.vector_weight as f32,
            config.keyword_weight as f32,
            config.embedding_cache_size,
            config.sqlite_open_timeout_secs,
        )?;
        Ok(mem)
    }

    let memory = create_memory_with_builders(
        &backend_name,
        || build_sqlite_memory(config, workspace_dir, &resolved_embedding),
        "",
    )?;

    // If snapshot_on_hygiene is enabled, export core memories during hygiene.
    // After the backend is built: opening it migrates the schema, and the export
    // selects on `session_id`, a column a database from before session ids lacks
    // until then. Hydration above has already run, so a cold boot exports the
    // restored rows back unchanged.
    if config.snapshot_enabled
        && config.snapshot_on_hygiene
        && matches!(backend_kind, MemoryBackendKind::Sqlite)
    {
        if let Err(e) = snapshot::export_snapshot(workspace_dir) {
            tracing::warn!("memory snapshot skipped: {e}");
        }
    }

    // Project core memory into `MEMORY.md`, which the system prompt injects.
    //
    // On these backends nothing else writes that file, so the tier guaranteed to
    // reach the model held only scaffold prose while everything the agent learned
    // lived in `brain.db`. `MarkdownMemory` is excluded because it owns the file
    // directly — projecting there too would write it twice.
    //
    // A channel reads the file for each message it answers. The interactive
    // session builds its prompt once, so a memory stored mid-session lands in the
    // file now and in that prompt next session; there, within-session freshness is
    // the recall tier's job, which runs every turn.
    if matches!(backend_kind, MemoryBackendKind::Sqlite) {
        if let Err(e) = snapshot::project_core_memories(workspace_dir) {
            tracing::warn!("memory projection skipped: {e}");
        }
    }

    Ok(memory)
}

/// Build the SQLite backend with the embedder the config asks for.
///
/// `create_memory_for_migration` deliberately skips embedding setup because its
/// callers only read and delete. Re-embedding needs the real provider, and it
/// needs the concrete type — `reindex` is not on the `Memory` trait, because
/// only a backend that stores vectors has anything to rebuild.
pub fn create_sqlite_with_embedder(
    config: &MemoryConfig,
    embedding_routes: &[EmbeddingRouteConfig],
    workspace_dir: &Path,
    api_key: Option<&str>,
) -> anyhow::Result<SqliteMemory> {
    let resolved = resolve_embedding_config(config, embedding_routes, api_key);
    let embedder: Arc<dyn embeddings::EmbeddingProvider> =
        Arc::from(embeddings::create_embedding_provider(
            &resolved.provider,
            resolved.api_key.as_deref(),
            &resolved.model,
            resolved.dimensions,
        ));

    #[allow(clippy::cast_possible_truncation)]
    SqliteMemory::with_embedder(
        workspace_dir,
        embedder,
        config.vector_weight as f32,
        config.keyword_weight as f32,
        config.embedding_cache_size,
        config.sqlite_open_timeout_secs,
    )
}

/// Build a backend without embedding setup, for work that only reads or deletes.
///
/// `purpose` names what the caller is doing, so an error about an unrecognised
/// backend does not claim a migration is underway when the operator ran
/// `memory stats`.
fn create_memory_without_embeddings(
    backend: &str,
    workspace_dir: &Path,
    purpose: &str,
) -> anyhow::Result<Box<dyn Memory>> {
    if matches!(classify_memory_backend(backend), MemoryBackendKind::None) {
        anyhow::bail!("memory backend 'none' disables persistence; choose sqlite to {purpose}");
    }

    create_memory_with_builders(backend, || SqliteMemory::new(workspace_dir), "")
}

pub fn create_memory_for_migration(
    backend: &str,
    workspace_dir: &Path,
) -> anyhow::Result<Box<dyn Memory>> {
    create_memory_without_embeddings(backend, workspace_dir, "migrate")
}

/// Backend for the `rantaiclaw memory` commands.
///
/// Skips embedding setup — these commands never run a vector search — and, unlike
/// the migration factory it used to borrow, reports errors in terms of what the
/// operator actually asked for.
pub fn create_memory_for_cli(
    backend: &str,
    workspace_dir: &Path,
) -> anyhow::Result<Box<dyn Memory>> {
    create_memory_without_embeddings(backend, workspace_dir, "manage memory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EmbeddingRouteConfig;
    use tempfile::TempDir;

    /// A database written before the `session_id` column existed has no such
    /// column until the backend opens it. The snapshot export selects on that
    /// column, so it has to run after the backend has migrated the schema, or the
    /// first start after an upgrade skips the export with "no such column".
    #[tokio::test]
    async fn the_snapshot_export_runs_after_the_schema_is_migrated() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&db_dir).unwrap();
        {
            let conn = rusqlite::Connection::open(db_dir.join("brain.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE memories (
                    id TEXT PRIMARY KEY,
                    key TEXT NOT NULL UNIQUE,
                    content TEXT NOT NULL,
                    category TEXT NOT NULL DEFAULT 'core',
                    embedding BLOB,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                INSERT INTO memories (id, key, content, category, created_at, updated_at)
                VALUES ('1', 'legacy_note', 'written before session ids', 'core',
                        '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');",
            )
            .unwrap();
        }
        let config = MemoryConfig {
            backend: "sqlite".to_string(),
            snapshot_enabled: true,
            snapshot_on_hygiene: true,
            auto_hydrate: false,
            ..MemoryConfig::default()
        };

        let _memory = create_memory_with_storage(&config, tmp.path(), None).unwrap();

        let snapshot = std::fs::read_to_string(tmp.path().join(snapshot::SNAPSHOT_FILENAME))
            .unwrap_or_default();
        assert!(
            snapshot.contains("legacy_note"),
            "the export did not run on a database from before session ids:\n{snapshot}"
        );
    }

    #[test]
    fn factory_sqlite() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "sqlite".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "sqlite");
    }

    #[test]
    fn assistant_autosave_key_detection_matches_legacy_patterns() {
        assert!(is_assistant_autosave_key("assistant_resp"));
        assert!(is_assistant_autosave_key("assistant_resp_1234"));
        assert!(is_assistant_autosave_key("ASSISTANT_RESP_abcd"));
        assert!(!is_assistant_autosave_key("assistant_response"));
        assert!(!is_assistant_autosave_key("user_msg_1234"));
    }

    #[test]
    fn factory_none_uses_noop_memory() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "none".into(),
            ..MemoryConfig::default()
        };
        let mem = create_memory(&cfg, tmp.path(), None).unwrap();
        assert_eq!(mem.name(), "none");
    }

    #[test]
    /// An unrecognised backend used to fall back to markdown behind a warning,
    /// so a typo silently ran a different store with different semantics. The
    /// backend name is a config contract; an unknown value is an error.
    fn factory_unknown_backend_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "redis".into(),
            ..MemoryConfig::default()
        };
        let error = create_memory(&cfg, tmp.path(), None)
            .err()
            .expect("an unknown backend must not silently resolve to another one");
        let message = error.to_string();
        assert!(message.contains("redis"), "{message}");
        assert!(
            message.contains("sqlite"),
            "the error must list the valid values: {message}"
        );
    }

    #[test]
    fn migration_factory_none_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let error = create_memory_for_migration("none", tmp.path())
            .err()
            .expect("backend=none should be rejected for migration");
        assert!(error.to_string().contains("disables persistence"));
    }

    /// The `lucid` backend was retired; use sqlite. A leftover config that
    /// still names `lucid` must not stop the daemon: resolve to `sqlite` so
    /// the backend the operator already has on disk keeps working.
    #[test]
    fn effective_backend_name_retired_lucid_resolves_to_sqlite() {
        assert_eq!(effective_memory_backend_name("lucid"), "sqlite");
    }

    /// The `markdown` backend was retired too; use sqlite. Migration
    /// already rewrote a config that said `markdown`; this is the runtime
    /// fallback for any value that slips past it.
    #[test]
    fn effective_backend_name_retired_markdown_resolves_to_sqlite() {
        assert_eq!(effective_memory_backend_name("markdown"), "sqlite");
    }

    /// The `postgres` backend was retired; use sqlite. It went together with the
    /// `[storage]` section it was the only user of. A leftover config that
    /// still names `postgres` must not stop the daemon: resolve to `sqlite`
    /// so the local store keeps working without reaching for a database the
    /// binary no longer ships the driver for.
    #[test]
    fn effective_backend_name_retired_postgres_resolves_to_sqlite() {
        assert_eq!(effective_memory_backend_name("postgres"), "sqlite");
    }

    /// A name that was never a backend must still error — the retire mapping
    /// is a soft transition for three specific values, not a free pass for any
    /// typo. Catches the regression where someone removes the Unknown arm.
    #[test]
    fn effective_backend_name_unknown_still_resolves_as_unknown() {
        // The mapping returns the trimmed/lowercased input verbatim; the
        // factory classifies it. We assert the factory still errors.
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "redis".into(),
            ..MemoryConfig::default()
        };
        let err = create_memory(&cfg, tmp.path(), None)
            .err()
            .expect("unknown backend must still error");
        assert!(
            err.to_string().contains("redis"),
            "the error names the bad value: {err}"
        );
        assert!(
            err.to_string().contains("sqlite") && err.to_string().contains("none"),
            "the error lists the only valid values: {err}"
        );
        assert!(
            !err.to_string().contains("lucid") && !err.to_string().contains("markdown"),
            "the error must not name retired backends: {err}"
        );
    }

    #[test]
    fn resolve_embedding_config_uses_base_config_when_model_is_not_hint() {
        let cfg = MemoryConfig {
            embedding_provider: "openai".into(),
            embedding_model: "text-embedding-3-small".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };

        let resolved = resolve_embedding_config(&cfg, &[], Some("base-key"));
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "openai".into(),
                model: "text-embedding-3-small".into(),
                dimensions: 1536,
                api_key: Some("base-key".into()),
            }
        );
    }

    #[test]
    fn resolve_embedding_config_uses_matching_route_with_api_key_override() {
        let cfg = MemoryConfig {
            embedding_provider: "none".into(),
            embedding_model: "hint:semantic".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };
        let routes = vec![EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "custom:https://api.example.com/v1".into(),
            model: "custom-embed-v2".into(),
            dimensions: Some(1024),
            api_key: Some("route-key".into()),
        }];

        let resolved = resolve_embedding_config(&cfg, &routes, Some("base-key"));
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "custom:https://api.example.com/v1".into(),
                model: "custom-embed-v2".into(),
                dimensions: 1024,
                api_key: Some("route-key".into()),
            }
        );
    }

    #[test]
    fn resolve_embedding_config_falls_back_when_hint_is_missing() {
        let cfg = MemoryConfig {
            embedding_provider: "openai".into(),
            embedding_model: "hint:semantic".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };

        let resolved = resolve_embedding_config(&cfg, &[], Some("base-key"));
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "openai".into(),
                model: "hint:semantic".into(),
                dimensions: 1536,
                api_key: Some("base-key".into()),
            }
        );
    }

    #[test]
    fn resolve_embedding_config_falls_back_when_route_is_invalid() {
        let cfg = MemoryConfig {
            embedding_provider: "openai".into(),
            embedding_model: "hint:semantic".into(),
            embedding_dimensions: 1536,
            ..MemoryConfig::default()
        };
        let routes = vec![EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: String::new(),
            model: "text-embedding-3-small".into(),
            dimensions: Some(0),
            api_key: None,
        }];

        let resolved = resolve_embedding_config(&cfg, &routes, Some("base-key"));
        assert_eq!(
            resolved,
            ResolvedEmbeddingConfig {
                provider: "openai".into(),
                model: "hint:semantic".into(),
                dimensions: 1536,
                api_key: Some("base-key".into()),
            }
        );
    }

    /// `keyword` is the only label the no-op backend can produce — it is the
    /// state `search_mode_label` reports when the factory short-circuits to
    /// `NoopEmbedding`. The CLI stats line, the recall header and the gateway
    /// `mode` field all read this fn; the absence of any provider name keeps
    /// them from claiming semantic search is on when it is not.
    #[test]
    fn search_mode_label_is_keyword_for_none_and_empty() {
        assert_eq!(search_mode_label("none"), "keyword");
        assert_eq!(search_mode_label(""), "keyword");
        assert_eq!(search_mode_label("  "), "keyword");
        assert_eq!(search_mode_label("NONE"), "keyword");
    }

    /// Any non-none name — known or unknown — is reported with the hybrid
    /// label and the provider name. The check does not validate the name;
    /// that is the doctor check's job. The surfaces that show the mode must
    /// not silently down-grade an unknown to `keyword`, because the daemon
    /// was started with the value and an operator may be reading it.
    #[test]
    fn search_mode_label_names_a_known_provider() {
        assert_eq!(search_mode_label("openai"), "keyword + semantic (openai)");
        assert_eq!(
            search_mode_label("custom:https://api.example.com"),
            "keyword + semantic (custom:https://api.example.com)"
        );
    }
}
