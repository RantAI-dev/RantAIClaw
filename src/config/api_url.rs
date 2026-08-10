//! Shape rules for `config.api_url`, shared by every surface that stores,
//! echoes, or diagnoses it.
//!
//! `api_url` is written to `config.toml` in plaintext, unlike `api_key`, which
//! is encrypted at rest. A credential that lands here is therefore stored
//! unprotected, echoed back to the web console's base-URL field, and
//! interpolated into operator-facing error messages. The gateway has rejected
//! credential-shaped writes since v0.18.0, but that guard is write-only: every
//! config that already held such a value kept it.
//!
//! Two distinct shapes, two distinct policies — the callers depend on the split:
//!
//! - [`looks_like_api_key`] — a credential. Never stored, never echoed,
//!   always warned about. Dropped at load ([`Config::load_or_init`]) and
//!   withheld by the gateway's `/secrets` view.
//! - [`validate_api_url`] — well-formedness. A merely malformed value is not a
//!   secret; it is kept and reported by `doctor` so the operator can see and
//!   correct it instead of having it vanish silently.

/// Prefixes used by the providers this project talks to. A value carrying one
/// of these was meant for `api_key`, whatever field it arrived in.
const API_KEY_PREFIXES: [&str; 6] = ["sk-", "sk_", "gsk_", "xai-", "AIza", "hf_"];

/// True when the value is shaped like a provider credential rather than a URL.
pub fn looks_like_api_key(value: &str) -> bool {
    let value = value.trim();
    API_KEY_PREFIXES
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

/// Reject anything that is not an `http`/`https` URL, naming the credential
/// case explicitly so the operator is told which field the value belongs in.
pub fn validate_api_url(value: &str) -> Result<(), String> {
    if looks_like_api_key(value) {
        return Err(
            "api_url looks like an API key, not a URL — set it as api_key instead".to_string(),
        );
    }

    let url = reqwest::Url::parse(value)
        .map_err(|_| "api_url must be a valid URL, e.g. https://api.example.com/v1".to_string())?;

    if !matches!(url.scheme(), "http" | "https") {
        return Err("api_url must use http:// or https://".to_string());
    }

    Ok(())
}

/// Remove a credential-shaped `api_url` from a raw config table, returning
/// `true` when something was dropped.
///
/// Operates on the raw TOML between read and parse, alongside the schema
/// migrations, so [`Config::load_or_init`]'s existing write-back also takes the
/// credential off disk rather than leaving it there until the next unrelated
/// save.
pub fn strip_credential_api_url(raw: &mut toml::Value) -> bool {
    let Some(table) = raw.as_table_mut() else {
        return false;
    };
    let is_credential = table
        .get("api_url")
        .and_then(toml::Value::as_str)
        .is_some_and(looks_like_api_key);

    if is_credential {
        table.remove("api_url");
    }
    is_credential
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> toml::Value {
        toml::from_str(toml_str).unwrap()
    }

    /// Derive the fixtures from the prefix table rather than spelling out
    /// realistic keys: a literal like `sk_live_<24 chars>` trips GitHub's push
    /// protection, and a hand-written list silently stops covering any prefix
    /// added later.
    #[test]
    fn credential_prefixes_are_recognised() {
        for prefix in API_KEY_PREFIXES {
            let value = format!("{prefix}EXAMPLE");
            assert!(
                looks_like_api_key(&value),
                "{value} must read as a credential"
            );
            assert!(
                validate_api_url(&value)
                    .expect_err("a credential-shaped api_url must be rejected")
                    .contains("API key"),
                "the message should name the confusion"
            );
        }
        assert!(
            looks_like_api_key("  sk-EXAMPLE  "),
            "surrounding whitespace must not hide a credential"
        );
    }

    #[test]
    fn urls_are_not_mistaken_for_credentials() {
        for value in [
            "https://openrouter.ai/api/v1",
            "http://localhost:8080/v1",
            "https://ollama.com",
            "not-a-url",
        ] {
            assert!(
                !looks_like_api_key(value),
                "{value} must not read as a credential"
            );
        }
    }

    #[test]
    fn validate_api_url_rejects_credentials_and_non_http_urls() {
        assert!(validate_api_url("sk-or-v1-EXAMPLE").is_err());
        assert!(validate_api_url("not a url at all").is_err());
        assert!(validate_api_url("ftp://api.example.com").is_err());
        assert!(validate_api_url("file:///etc/passwd").is_err());
        assert!(validate_api_url("http://localhost:8080/v1").is_ok());
        assert!(validate_api_url("https://api.example.com/v1").is_ok());
    }

    #[test]
    fn strip_removes_a_credential_shaped_api_url() {
        let mut raw = parse("api_url = \"sk-or-v1-EXAMPLE\"\ndefault_provider = \"openrouter\"\n");

        assert!(strip_credential_api_url(&mut raw));
        assert!(
            raw.get("api_url").is_none(),
            "the credential must be gone from the raw config"
        );
        assert_eq!(
            raw.get("default_provider").and_then(toml::Value::as_str),
            Some("openrouter"),
            "stripping must not disturb neighbouring keys"
        );
    }

    #[test]
    fn strip_keeps_a_real_endpoint() {
        let mut raw = parse("api_url = \"http://localhost:8080/v1\"\n");

        assert!(!strip_credential_api_url(&mut raw));
        assert_eq!(
            raw.get("api_url").and_then(toml::Value::as_str),
            Some("http://localhost:8080/v1")
        );
    }

    /// A typo is not a secret: dropping it would silently switch the provider
    /// back to its default endpoint and hide the operator's mistake. `doctor`
    /// reports this shape instead.
    #[test]
    fn strip_keeps_a_malformed_value_for_doctor_to_report() {
        let mut raw = parse("api_url = \"not-a-url\"\n");

        assert!(!strip_credential_api_url(&mut raw));
        assert_eq!(
            raw.get("api_url").and_then(toml::Value::as_str),
            Some("not-a-url")
        );
    }

    #[test]
    fn strip_is_idempotent_and_ignores_a_missing_key() {
        let mut raw = parse("default_provider = \"openrouter\"\n");

        assert!(!strip_credential_api_url(&mut raw));
        assert!(!strip_credential_api_url(&mut raw));
    }
}
