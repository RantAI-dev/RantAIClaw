//! Gemini CLI OAuth credential detection.
//!
//! One reader for `~/.gemini/oauth_creds.json` — the file Gemini CLI writes and
//! the only Gemini credential that is not a key the operator typed. The
//! onboarding wizard uses it to offer "reuse your existing Gemini CLI login"
//! during provider setup, independent of which Gemini provider path (legacy or
//! `RigProvider`) ends up handling inference, and the provider factory uses it
//! to refuse a login that has expired before any request is sent.

use directories::UserDirs;
use serde::Deserialize;
use std::path::PathBuf;

/// The margin a token must have left on it to be used: 30 seconds, the same
/// margin the Qwen path already uses, so a token that dies in transit is not
/// sent.
const TOKEN_MARGIN_MILLIS: i64 = 30_000;

/// What to say when the only Gemini credential is an expired CLI login.
///
/// Names the cause and both fixes: renew the login with `gemini`, or set
/// `GEMINI_API_KEY`.
pub const EXPIRED_LOGIN_REFUSAL: &str =
    "the Gemini CLI login has expired — run `gemini` to renew it, or set `GEMINI_API_KEY`";

/// OAuth token stored by Gemini CLI in `~/.gemini/oauth_creds.json`.
///
/// Gemini CLI writes `JSON.stringify(Credentials)`, and `Credentials` in
/// `google-auth-library` carries `expiry_date?: number | null` — the time in
/// milliseconds since the epoch at which the access token expires. There is no
/// field named `expiry` in that file, so a reader that only knows `expiry`
/// never sees an expiry at all and hands out a dead token. The `expiry` string
/// is still read, because a test and possibly an older tool wrote that shape.
#[derive(Debug, Deserialize)]
struct GeminiCliOAuthCreds {
    access_token: Option<String>,
    /// Milliseconds since the epoch — the shape Gemini CLI writes.
    #[serde(default, deserialize_with = "deserialize_expiry_millis")]
    expiry_date: Option<i64>,
    /// RFC 3339 timestamp — the older shape.
    expiry: Option<String>,
}

/// The Gemini CLI login, as the reader sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeminiCliLogin {
    /// No login file, or no access token in it.
    Missing,
    /// A token that is still valid.
    Valid,
    /// A token that has expired. The file is there; the login is dead.
    Expired,
}

/// What the reader found in the login file.
enum Login {
    /// No file, or no access token in it.
    Missing,
    /// A usable token.
    Valid(String),
    /// A token that has expired.
    Expired,
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

/// Read the login file and decide what to do with what it holds.
fn read_login() -> Login {
    let Some(creds) = read_creds() else {
        return Login::Missing;
    };
    let Some(token) = creds.access_token.as_deref().and_then(normalize_non_empty) else {
        return Login::Missing;
    };
    if token_is_expired(&creds) {
        Login::Expired
    } else {
        Login::Valid(token)
    }
}

/// Read the credentials file Gemini CLI writes.
fn read_creds() -> Option<GeminiCliOAuthCreds> {
    let gemini_dir = gemini_cli_dir()?;
    let creds_path = gemini_dir.join("oauth_creds.json");
    if !creds_path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&creds_path).ok()?;
    serde_json::from_str::<GeminiCliOAuthCreds>(&content).ok()
}

/// What the Gemini CLI login file says.
pub fn gemini_cli_login() -> GeminiCliLogin {
    match read_login() {
        Login::Missing => GeminiCliLogin::Missing,
        Login::Valid(_) => GeminiCliLogin::Valid,
        Login::Expired => GeminiCliLogin::Expired,
    }
}

/// Try to load the OAuth access token from Gemini CLI's cached credentials.
/// Location: `~/.gemini/oauth_creds.json`.
///
/// An expired token yields `None`: it is not sent to Google, where it would
/// come back as a 401.
pub fn try_load_gemini_cli_token() -> Option<String> {
    match read_login() {
        Login::Valid(token) => Some(token),
        Login::Missing | Login::Expired => None,
    }
}

/// Check if Gemini CLI is configured and has valid (non-expired) credentials.
pub fn gemini_cli_has_credentials() -> bool {
    matches!(read_login(), Login::Valid(_))
}

/// Whether the token in the file has expired.
///
/// `expiry_date` (milliseconds since the epoch) decides when it is there; the
/// older `expiry` string is the fallback. A file with no expiry at all keeps
/// working: Gemini CLI can write a partial file, and refusing it would break a
/// login that works.
fn token_is_expired(creds: &GeminiCliOAuthCreds) -> bool {
    if let Some(expiry_millis) = creds.expiry_date {
        return expiry_millis <= now_millis().saturating_add(TOKEN_MARGIN_MILLIS);
    }
    if let Some(expiry) = creds.expiry.as_deref() {
        if let Ok(expiry_time) = chrono::DateTime::parse_from_rfc3339(expiry) {
            let margin = chrono::Duration::milliseconds(TOKEN_MARGIN_MILLIS);
            return expiry_time <= chrono::Utc::now() + margin;
        }
    }
    false
}

/// Milliseconds since the epoch.
///
/// `0` when the clock reads before the epoch: every expiry is then in the
/// future, so nothing is refused on a clock that cannot be trusted.
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// The magnitude a float `expiry_date` must stay below to still name a
/// deadline an `i64` of milliseconds can hold: about 9.22e18 milliseconds is
/// the year 292 million, so no real deadline is near it.
///
/// Written as an `f64` literal rather than `i64::MAX as f64`: the cast trips
/// `clippy::cast_precision_loss`, a pedantic lint the crate root allows.
const MAX_EXPIRY_MILLIS_ABS: f64 = 9.223_372_036_854_776e18;

/// Read `expiry_date` — milliseconds since the epoch — tolerantly.
///
/// Gemini CLI writes a whole number. A file written by another tool may carry a
/// float or a numeric string; neither may make the whole file unreadable,
/// because that would break a login that works.
///
/// A float that cannot name a deadline at all — not a finite number, or beyond
/// the magnitude an `i64` of milliseconds holds — is left out rather than
/// saturated. A saturated value says "expires at the end of time", and it
/// would hide the `expiry` string that says the login is already dead.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a float expiry is milliseconds with a fractional part; truncating it to whole milliseconds keeps the deadline it names, and a float that cannot name a deadline is left out below"
)]
fn deserialize_expiry_millis<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match raw {
        Some(serde_json::Value::Number(number)) => number.as_i64().or_else(|| {
            number
                .as_f64()
                .filter(|value| value.is_finite() && value.abs() < MAX_EXPIRY_MILLIS_ABS)
                .map(|value| value as i64)
        }),
        Some(serde_json::Value::String(text)) => text.trim().parse::<i64>().ok(),
        _ => None,
    })
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

    /// Point `HOME` at a fresh temp directory and create `~/.gemini` in it.
    ///
    /// The guard restores `HOME` when it is dropped, so a panicking test does
    /// not leak the override into the next one sharing `ENV_LOCK`.
    fn gemini_home() -> (tempfile::TempDir, crate::test_env::HomeGuard) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let guard = crate::test_env::HomeGuard::set(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".gemini")).expect("create .gemini dir");
        (tmp, guard)
    }

    /// The path of the credentials file inside a temp home.
    fn creds_path(home: &tempfile::TempDir) -> std::path::PathBuf {
        home.path().join(".gemini").join("oauth_creds.json")
    }

    /// The shape Gemini CLI writes: `expiry_date` in milliseconds since the
    /// epoch.
    fn write_creds(home: &tempfile::TempDir, body: &str) {
        std::fs::write(creds_path(home), body).expect("write creds");
    }

    /// The reproduction: the token expired an hour ago, in the shape Gemini
    /// CLI writes. The reader must not hand it out.
    #[tokio::test]
    async fn an_expired_token_in_the_gemini_cli_shape_is_not_handed_out() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        let an_hour_ago = now_millis() - 3_600_000;
        write_creds(
            &home,
            &format!(r#"{{"access_token":"t","refresh_token":"r","expiry_date":{an_hour_ago}}}"#),
        );

        assert_eq!(gemini_cli_login(), GeminiCliLogin::Expired);
        assert!(!gemini_cli_has_credentials());
        assert_eq!(try_load_gemini_cli_token(), None);
    }

    /// The same shape, one hour ahead: the token is used.
    #[tokio::test]
    async fn a_token_in_the_gemini_cli_shape_that_is_still_valid_is_used() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        let an_hour_ahead = now_millis() + 3_600_000;
        write_creds(
            &home,
            &format!(r#"{{"access_token":"t","refresh_token":"r","expiry_date":{an_hour_ahead}}}"#),
        );

        assert_eq!(gemini_cli_login(), GeminiCliLogin::Valid);
        assert!(gemini_cli_has_credentials());
        assert_eq!(try_load_gemini_cli_token(), Some("t".to_string()));
    }

    /// A token that dies in ten seconds is not sent: the margin covers the
    /// request in flight.
    #[tokio::test]
    async fn a_token_that_dies_in_ten_seconds_is_not_used() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        let in_ten_seconds = now_millis() + 10_000;
        write_creds(
            &home,
            &format!(r#"{{"access_token":"t","expiry_date":{in_ten_seconds}}}"#),
        );

        assert_eq!(gemini_cli_login(), GeminiCliLogin::Expired);
        assert_eq!(try_load_gemini_cli_token(), None);
    }

    /// The older shape still works, both ways.
    #[tokio::test]
    async fn the_older_expiry_string_still_works_both_ways() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        write_creds(
            &home,
            r#"{"access_token":"expired-token","expiry":"2000-01-01T00:00:00Z"}"#,
        );
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Expired);
        assert_eq!(try_load_gemini_cli_token(), None);

        write_creds(
            &home,
            r#"{"access_token":"valid-token","expiry":"2999-01-01T00:00:00Z"}"#,
        );
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Valid);
        assert_eq!(try_load_gemini_cli_token(), Some("valid-token".to_string()));
    }

    /// Both fields present: `expiry_date` decides.
    #[tokio::test]
    async fn expiry_date_decides_when_both_fields_are_present() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        let an_hour_ago = now_millis() - 3_600_000;
        write_creds(
            &home,
            &format!(
                r#"{{"access_token":"t","expiry":"2999-01-01T00:00:00Z","expiry_date":{an_hour_ago}}}"#
            ),
        );
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Expired);

        let an_hour_ahead = now_millis() + 3_600_000;
        write_creds(
            &home,
            &format!(
                r#"{{"access_token":"t","expiry":"2000-01-01T00:00:00Z","expiry_date":{an_hour_ahead}}}"#
            ),
        );
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Valid);
    }

    /// A float `expiry_date` that cannot be a deadline is left out, so the
    /// `expiry` string decides. A saturated number would say "expires at the
    /// end of time" and hide a login that is already dead.
    #[tokio::test]
    async fn a_float_expiry_date_beyond_the_range_is_left_out_so_the_expiry_string_decides() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        // `1e30` milliseconds is no deadline at all, and no number an `i64` of
        // milliseconds can hold.
        write_creds(
            &home,
            r#"{"access_token":"t","expiry":"2000-01-01T00:00:00Z","expiry_date":1e30}"#,
        );
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Expired);
        assert!(!gemini_cli_has_credentials());
        assert_eq!(try_load_gemini_cli_token(), None);

        // The string decides in the other direction too: a login the `expiry`
        // string says is alive is alive.
        write_creds(
            &home,
            r#"{"access_token":"t","expiry":"2999-01-01T00:00:00Z","expiry_date":1e30}"#,
        );
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Valid);
        assert_eq!(try_load_gemini_cli_token(), Some("t".to_string()));
    }

    /// A float within the range still decides, both ways.
    #[tokio::test]
    async fn a_float_expiry_date_within_the_range_still_decides() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        let an_hour_ahead = now_millis() + 3_600_000;
        write_creds(
            &home,
            &format!(r#"{{"access_token":"t","expiry_date":{an_hour_ahead}.5}}"#),
        );
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Valid);
        assert_eq!(try_load_gemini_cli_token(), Some("t".to_string()));

        let an_hour_ago = now_millis() - 3_600_000;
        write_creds(
            &home,
            &format!(r#"{{"access_token":"t","expiry_date":{an_hour_ago}.5}}"#),
        );
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Expired);
        assert_eq!(try_load_gemini_cli_token(), None);
    }

    /// A float with a fractional part is truncated to whole milliseconds, so a
    /// deadline at the margin is refused exactly like the whole-millisecond
    /// deadline it names.
    #[tokio::test]
    async fn a_float_expiry_date_is_truncated_to_whole_milliseconds() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        let at_the_margin = now_millis() + TOKEN_MARGIN_MILLIS;
        write_creds(
            &home,
            &format!(r#"{{"access_token":"t","expiry_date":{at_the_margin}}}"#),
        );
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Expired);

        // The same deadline with a fractional part truncates to the same whole
        // millisecond, and is refused the same way.
        write_creds(
            &home,
            &format!(r#"{{"access_token":"t","expiry_date":{at_the_margin}.5}}"#),
        );
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Expired);
        assert_eq!(try_load_gemini_cli_token(), None);
    }

    /// A negative float is a deadline before the epoch, and it is honoured:
    /// the bound is on the magnitude, not on the sign.
    #[tokio::test]
    async fn a_negative_float_expiry_date_is_still_a_deadline() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        // An hour before the epoch, with a fractional part.
        write_creds(&home, r#"{"access_token":"t","expiry_date":-3600000.5}"#);
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Expired);
        assert_eq!(try_load_gemini_cli_token(), None);
    }

    /// A file with no expiry at all keeps working: Gemini CLI can write a
    /// partial file, and refusing it would break a login that works.
    #[tokio::test]
    async fn a_file_without_an_expiry_keeps_working() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        write_creds(&home, r#"{"access_token":"t"}"#);
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Valid);
        assert_eq!(try_load_gemini_cli_token(), Some("t".to_string()));

        // `expiry_date: null` is the shape Gemini CLI writes when the token
        // has no known expiry.
        write_creds(&home, r#"{"access_token":"t","expiry_date":null}"#);
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Valid);
        assert_eq!(try_load_gemini_cli_token(), Some("t".to_string()));
    }

    /// A file that is not JSON, or that holds no token, yields no token — and
    /// does not panic.
    #[tokio::test]
    async fn a_broken_or_empty_file_yields_no_token() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let (home, _home_guard) = gemini_home();

        write_creds(&home, "not json at all");
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Missing);
        assert_eq!(try_load_gemini_cli_token(), None);

        write_creds(&home, r#"{"refresh_token":"r"}"#);
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Missing);
        assert_eq!(try_load_gemini_cli_token(), None);

        // An empty access token is no token.
        write_creds(&home, r#"{"access_token":"   "}"#);
        assert_eq!(gemini_cli_login(), GeminiCliLogin::Missing);
        assert_eq!(try_load_gemini_cli_token(), None);
    }

    /// The provider uses the shared reader: a file in the Gemini CLI shape
    /// decides what it holds.
    #[tokio::test]
    async fn the_provider_uses_the_shared_reader() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let (home, _home_guard) = gemini_home();

        let an_hour_ago = now_millis() - 3_600_000;
        write_creds(
            &home,
            &format!(r#"{{"access_token":"t","expiry_date":{an_hour_ago}}}"#),
        );
        let provider = crate::providers::gemini::GeminiProvider::new(None);
        assert_eq!(provider.auth_source(), "none");

        let an_hour_ahead = now_millis() + 3_600_000;
        write_creds(
            &home,
            &format!(r#"{{"access_token":"t","expiry_date":{an_hour_ahead}}}"#),
        );
        let provider = crate::providers::gemini::GeminiProvider::new(None);
        assert_eq!(provider.auth_source(), "Gemini CLI OAuth");
    }

    /// The refusal names the command to run and the key to set. The provider
    /// is only built — no request is sent.
    #[tokio::test]
    async fn an_expired_login_is_refused_with_the_fix_named() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let (home, _home_guard) = gemini_home();

        let an_hour_ago = now_millis() - 3_600_000;
        write_creds(
            &home,
            &format!(r#"{{"access_token":"t","expiry_date":{an_hour_ago}}}"#),
        );

        let error = match crate::providers::create_provider("gemini", None) {
            Ok(_) => panic!("an expired Gemini CLI login must be refused"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("gemini"), "{message}");
        assert!(message.contains("GEMINI_API_KEY"), "{message}");
    }
}
