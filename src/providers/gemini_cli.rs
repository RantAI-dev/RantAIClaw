//! Gemini CLI OAuth credential detection.
//!
//! Extracted out of the legacy `gemini::GeminiProvider` (behind
//! `--features legacy-providers`) so it's reachable from default builds
//! without pulling in the legacy struct. The onboarding wizard uses this to
//! offer "reuse your existing Gemini CLI login" during provider setup,
//! independent of which Gemini provider path (legacy or `RigProvider`) ends
//! up handling inference.

use directories::UserDirs;
use serde::Deserialize;
use std::path::PathBuf;

/// OAuth token stored by Gemini CLI in `~/.gemini/oauth_creds.json`.
#[derive(Debug, Deserialize)]
struct GeminiCliOAuthCreds {
    access_token: Option<String>,
    expiry: Option<String>,
}

/// Get the Gemini CLI config directory (`~/.gemini`).
fn gemini_cli_dir() -> Option<PathBuf> {
    UserDirs::new().map(|u| u.home_dir().join(".gemini"))
}

fn normalize_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Try to load the OAuth access token from Gemini CLI's cached credentials.
/// Location: `~/.gemini/oauth_creds.json`.
pub fn try_load_gemini_cli_token() -> Option<String> {
    let gemini_dir = gemini_cli_dir()?;
    let creds_path = gemini_dir.join("oauth_creds.json");

    if !creds_path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&creds_path).ok()?;
    let creds: GeminiCliOAuthCreds = serde_json::from_str(&content).ok()?;

    // Check if token is expired (basic check).
    if let Some(ref expiry) = creds.expiry {
        if let Ok(expiry_time) = chrono::DateTime::parse_from_rfc3339(expiry) {
            if expiry_time < chrono::Utc::now() {
                tracing::warn!("Gemini CLI OAuth token expired — re-run `gemini` to refresh");
                return None;
            }
        }
    }

    creds
        .access_token
        .and_then(|token| normalize_non_empty(&token))
}

/// Check if Gemini CLI is configured and has valid (non-expired) credentials.
pub fn gemini_cli_has_credentials() -> bool {
    try_load_gemini_cli_token().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_cli_dir_returns_path() {
        let dir = gemini_cli_dir();
        // `UserDirs::new()` only fails on exotic systems without a resolvable
        // home dir; on any normal CI/dev box this should resolve, and if it
        // does, it must end in `.gemini`.
        if UserDirs::new().is_some() {
            assert!(dir.is_some());
            assert!(dir.unwrap().ends_with(".gemini"));
        }
    }

    /// Write `~/.gemini/oauth_creds.json` under a temp `HOME` and assert
    /// `gemini_cli_has_credentials()` reflects the token's expiry state.
    /// Serialized via the crate-shared `ENV_LOCK` because `HOME` is a
    /// process-global env var read by unrelated tests too.
    #[tokio::test]
    async fn gemini_cli_has_credentials_reflects_token_expiry() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let original_home = std::env::var_os("HOME");

        let tmp = tempfile::TempDir::new().expect("tempdir");
        std::env::set_var("HOME", tmp.path());

        // No ~/.gemini directory yet.
        assert!(!gemini_cli_has_credentials());

        let gemini_dir = tmp.path().join(".gemini");
        std::fs::create_dir_all(&gemini_dir).expect("create .gemini dir");
        let creds_path = gemini_dir.join("oauth_creds.json");

        // Expired token -> no credentials.
        std::fs::write(
            &creds_path,
            r#"{"access_token": "expired-token", "expiry": "2000-01-01T00:00:00Z"}"#,
        )
        .expect("write expired creds");
        assert!(!gemini_cli_has_credentials());
        assert_eq!(try_load_gemini_cli_token(), None);

        // Valid (far-future) token -> credentials detected.
        std::fs::write(
            &creds_path,
            r#"{"access_token": "valid-token", "expiry": "2999-01-01T00:00:00Z"}"#,
        )
        .expect("write valid creds");
        assert!(gemini_cli_has_credentials());
        assert_eq!(try_load_gemini_cli_token(), Some("valid-token".to_string()));

        match original_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
