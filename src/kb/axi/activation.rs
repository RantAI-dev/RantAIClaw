//! What `rantaiclaw kb enable|disable` and the TUI `/kb` command share:
//! where the embedding key comes from, whether the embedding endpoint
//! works, and a toggle that reports the state it actually left on disk.

use std::time::Duration;

use crate::config::Config;
use crate::kb::{embed, KbConfig, KbError, KbResult};

/// Upper bound for the whole probe, retries included.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Where the embedding key resolves from, in precedence order:
/// `KB_EMBEDDING_API_KEY`, the config value, then `OPENROUTER_API_KEY`.
/// Returns `"none"` when nothing resolves. Never returns the key itself.
///
/// The gateway, `kb status` and `/kb` all call this one function, so they
/// agree on whether a key resolves for the same config and environment.
pub fn embedding_key_source(config_key: Option<&str>) -> &'static str {
    let env_set = |var: &str| std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false);
    if env_set("KB_EMBEDDING_API_KEY") {
        "env"
    } else if config_key.is_some_and(|k| !k.is_empty()) {
        "config"
    } else if env_set("OPENROUTER_API_KEY") {
        "openrouter_env"
    } else {
        "none"
    }
}

/// Embed one short string through the configured provider and check the
/// vector size. `Err` carries a reason that is safe to show: it never holds
/// the key or the upstream response body (the variants that can carry one are
/// replaced by a fixed sentence).
pub async fn probe_embedding(cfg: &KbConfig) -> Result<(), String> {
    let provider = embed::make_provider(cfg).map_err(|e| e.to_string())?;
    let texts = ["ping".to_string()];
    let probe = provider.embed_many(&texts);
    match tokio::time::timeout(PROBE_TIMEOUT, probe).await {
        Err(_) => Err(format!(
            "the embedding endpoint did not answer within {} seconds",
            PROBE_TIMEOUT.as_secs()
        )),
        Ok(Ok(vectors)) if vectors.len() == 1 && vectors[0].len() == cfg.embedding_dim => Ok(()),
        Ok(Ok(_)) => Err("the embedding endpoint returned no usable vector".to_string()),
        Ok(Err(KbError::DimensionMismatch { expected, got, .. })) => Err(format!(
            "the embedding endpoint returns {got}-dimension vectors but KB_EMBEDDING_DIM is \
             {expected}; set KB_EMBEDDING_DIM to {got} or choose another embedding model"
        )),
        Ok(Err(KbError::EmbeddingApi { status, .. })) => Err(format!(
            "the embedding endpoint answered http {status}; check the key, the model and \
             KB_EMBEDDING_BASE_URL"
        )),
        Ok(Err(KbError::Http(e))) if e.is_decode() => Err(
            "the embedding endpoint answered with something that is not an embedding list"
                .to_string(),
        ),
        Ok(Err(KbError::Http(e))) if e.is_connect() || e.is_timeout() => Err(format!(
            "could not reach the embedding endpoint: {}",
            e.without_url()
        )),
        Ok(Err(KbError::Http(e))) => {
            Err(format!("the embedding request failed: {}", e.without_url()))
        }
        // These carry the upstream's response (a truncated JSON dump) or a
        // parser's words, so the reason is ours.
        Ok(Err(KbError::Other(_) | KbError::Json(_) | KbError::LengthMismatch { .. })) => Err(
            "the embedding endpoint answered with something that is not an embedding list"
                .to_string(),
        ),
        Ok(Err(e)) => Err(e.to_string()),
    }
}

/// Outcome of [`set_enabled`].
#[derive(Debug, PartialEq, Eq)]
pub enum Activation {
    /// The value read back from `config.toml` after the save.
    Persisted(bool),
    /// Nothing was changed, or the change did not stick.
    Refused { code: &'static str, message: String },
}

/// Turn the knowledge base on or off in `config.toml`.
///
/// Enabling first needs a key that resolves and an embedding endpoint that
/// answers with vectors of the configured size. The write goes through a
/// config loaded without environment overrides: the saver drops any value
/// that equals what the environment produced, and `KB_EMBEDDING_API_KEY`
/// already sets `enabled = true`, so a write through an env-folded config
/// would leave `false` on disk. The file is read again after the save and
/// the caller gets what it holds, not what was requested.
pub async fn set_enabled(enabled: bool) -> KbResult<Activation> {
    // Resolves the profile path; the env-folded values are only used for the
    // pre-checks below.
    let resolved = Config::load_or_init()
        .await
        .map_err(|e| KbError::Other(format!("config load: {e}")))?;
    let path = resolved.config_path.clone();

    if enabled {
        let key = resolved.knowledge.embedding_api_key.as_deref();
        if embedding_key_source(key) == "none" {
            return Ok(Activation::Refused {
                code: "no_credential",
                message: "cannot activate the knowledge base without an embedding key; \
                          run `rantaiclaw setup knowledge` or set KB_EMBEDDING_API_KEY"
                    .to_string(),
            });
        }
        let kb_cfg = KbConfig::from_env_with_keys(key, None)?;
        if let Err(message) = probe_embedding(&kb_cfg).await {
            return Ok(Activation::Refused {
                code: "embedding_unavailable",
                message,
            });
        }
    }

    let mut fresh = Config::load_from_path(&path)
        .await
        .map_err(|e| KbError::Other(format!("config load: {e}")))?;
    fresh.knowledge.enabled = enabled;
    fresh
        .save()
        .await
        .map_err(|e| KbError::Other(format!("config save: {e}")))?;

    let persisted = Config::load_from_path(&path)
        .await
        .map_err(|e| KbError::Other(format!("config reload: {e}")))?
        .knowledge
        .enabled;
    if persisted != enabled {
        return Ok(Activation::Refused {
            code: "not_persisted",
            message: format!(
                "the knowledge base is still {} in {}",
                if persisted { "enabled" } else { "disabled" },
                path.display()
            ),
        });
    }
    Ok(Activation::Persisted(persisted))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_env::{EnvGuard, ENV_LOCK};

    #[test]
    fn embedding_key_source_follows_env_then_config_then_openrouter() {
        let _lock = ENV_LOCK.blocking_lock();
        let _kb = EnvGuard::unset("KB_EMBEDDING_API_KEY");
        let _or = EnvGuard::unset("OPENROUTER_API_KEY");

        assert_eq!(embedding_key_source(None), "none");
        assert_eq!(embedding_key_source(Some("")), "none");
        assert_eq!(embedding_key_source(Some("rantaiclaw_test_key")), "config");

        {
            let _or = EnvGuard::set("OPENROUTER_API_KEY", "rantaiclaw_test_key");
            assert_eq!(embedding_key_source(None), "openrouter_env");
            assert_eq!(
                embedding_key_source(Some("rantaiclaw_test_key")),
                "config",
                "a config key outranks OPENROUTER_API_KEY"
            );
            let _kb = EnvGuard::set("KB_EMBEDDING_API_KEY", "rantaiclaw_test_key");
            assert_eq!(
                embedding_key_source(Some("rantaiclaw_test_key")),
                "env",
                "KB_EMBEDDING_API_KEY outranks the config value"
            );
        }
    }
}
