use rantaiclaw::kb::KbConfig;
use std::sync::Mutex;

// Process-wide guard: these tests mutate `KB_*` env vars and would race
// if run in parallel inside the same test binary. Using a parking-lot-free
// std Mutex keeps the dep surface narrow.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn defaults_match_ts_kb() {
    let _guard = ENV_LOCK.lock().unwrap();
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
    unsafe {
        std::env::set_var("KB_EMBEDDING_MODEL", "voyage/voyage-3");
        std::env::set_var("KB_EMBEDDING_DIM", "1024");
        std::env::set_var("KB_HYBRID_BM25_ENABLED", "false");
    }
    let cfg = KbConfig::from_env().expect("env config");
    assert_eq!(cfg.embedding_model, "voyage/voyage-3");
    assert_eq!(cfg.embedding_dim, 1024);
    assert!(!cfg.hybrid_bm25_enabled);
    unsafe {
        std::env::remove_var("KB_EMBEDDING_MODEL");
        std::env::remove_var("KB_EMBEDDING_DIM");
        std::env::remove_var("KB_HYBRID_BM25_ENABLED");
    }
}

#[test]
fn invalid_int_returns_error() {
    let _guard = ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::set_var("KB_EMBEDDING_DIM", "not-a-number");
    }
    let result = KbConfig::from_env();
    assert!(result.is_err());
    unsafe {
        std::env::remove_var("KB_EMBEDDING_DIM");
    }
}
