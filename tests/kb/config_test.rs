use rantaiclaw::kb::KbConfig;
use std::sync::Mutex;

// Process-wide guard: these tests mutate `KB_*` env vars and would race
// if run in parallel inside the same test binary. Using a parking-lot-free
// std Mutex keeps the dep surface narrow.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that removes the listed env vars on drop. Ensures cleanup
/// still runs if an assertion panics between `set_var` and the end of
/// the test — without this the env var leaks process-wide AND the
/// `ENV_LOCK` mutex becomes poisoned, breaking every subsequent test.
struct EnvGuard(Vec<&'static str>);
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for k in &self.0 {
            // SAFETY: single-threaded access serialized via ENV_LOCK above.
            unsafe {
                std::env::remove_var(k);
            }
        }
    }
}

#[test]
fn defaults_match_ts_kb() {
    // Tolerate a poisoned mutex: poisoning means an earlier test panicked
    // while holding the lock, but the env state is already restored by
    // each test's `EnvGuard` drop, so the protected state is sound. Without
    // this, an unrelated assertion failure cascades as a misleading
    // "poisoned mutex" panic here.
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Ensure no KB_* env vars leak into the test.
    for (k, _) in std::env::vars() {
        if k.starts_with("KB_") {
            unsafe {
                std::env::remove_var(&k);
            }
        }
    }
    let cfg = KbConfig::from_env().expect("default config");

    assert_eq!(cfg.extract_primary, "smart");
    assert_eq!(cfg.extract_fallback, "unpdf");
    assert_eq!(cfg.embedding_model, "qwen/qwen3-embedding-8b");
    assert_eq!(cfg.embedding_dim, 4096);
    assert_eq!(cfg.default_max_chunks, 8);
    assert!(!cfg.rerank_enabled);
    assert_eq!(cfg.rerank_model, "openai/gpt-4.1-nano");
    assert_eq!(cfg.rerank_initial_k, 20);
    assert_eq!(cfg.rerank_final_k, 5);
    assert!(cfg.hybrid_bm25_enabled);
    assert!(!cfg.contextual_retrieval_enabled);
    assert!(!cfg.query_expansion_enabled);
    assert_eq!(cfg.query_expansion_paraphrases, 3);
    assert_eq!(
        cfg.embedding_base_url,
        "https://openrouter.ai/api/v1/embeddings"
    );
    assert_eq!(
        cfg.extract_vision_base_url,
        "https://openrouter.ai/api/v1/chat/completions"
    );
}

#[test]
fn env_overrides_apply() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _env = EnvGuard(vec![
        "KB_EMBEDDING_MODEL",
        "KB_EMBEDDING_DIM",
        "KB_HYBRID_BM25_ENABLED",
    ]);
    unsafe {
        std::env::set_var("KB_EMBEDDING_MODEL", "voyage/voyage-3");
        std::env::set_var("KB_EMBEDDING_DIM", "1024");
        std::env::set_var("KB_HYBRID_BM25_ENABLED", "false");
    }
    let cfg = KbConfig::from_env().expect("env config");
    assert_eq!(cfg.embedding_model, "voyage/voyage-3");
    assert_eq!(cfg.embedding_dim, 1024);
    assert!(!cfg.hybrid_bm25_enabled);
    // Cleanup runs via EnvGuard::drop, even on assertion panic above.
}

#[test]
fn invalid_int_returns_error() {
    let _guard = ENV_LOCK.lock().unwrap();
    let _env = EnvGuard(vec!["KB_EMBEDDING_DIM"]);
    unsafe {
        std::env::set_var("KB_EMBEDDING_DIM", "not-a-number");
    }
    let result = KbConfig::from_env();
    assert!(result.is_err());
    // Cleanup runs via EnvGuard::drop.
}
