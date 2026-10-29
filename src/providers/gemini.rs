//! Google Gemini provider with support for:
//! - Direct API key (`GEMINI_API_KEY` env var or config)
//! - Gemini CLI OAuth tokens (reuse existing ~/.gemini/ authentication)
//! - Google Cloud ADC (`GOOGLE_APPLICATION_CREDENTIALS`)
//!
//! The Gemini CLI login is read by [`crate::providers::gemini_cli`] — one
//! reader for `~/.gemini/oauth_creds.json`, so the expiry check cannot drift
//! apart between this provider and the factory that builds it. The login is
//! read when a request is sent, not when the provider is built: a login
//! renewed by running `gemini` is used on the next request, and an expired
//! one fails that request with the fix named, before anything is sent.

use crate::providers::traits::{ChatMessage, Provider};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Gemini provider supporting multiple authentication methods.
pub struct GeminiProvider {
    auth: Option<GeminiAuth>,
}

/// Resolved credential — the variant determines both the HTTP auth method
/// and the diagnostic label returned by `auth_source()`.
#[derive(Debug)]
enum GeminiAuth {
    /// Explicit API key from config: sent as `?key=` query parameter.
    ExplicitKey(String),
    /// API key from `GEMINI_API_KEY` env var: sent as `?key=`.
    EnvGeminiKey(String),
    /// API key from `GOOGLE_API_KEY` env var: sent as `?key=`.
    EnvGoogleKey(String),
    /// The Gemini CLI login: read when a request is sent, sent as
    /// `Authorization: Bearer`.
    CliLogin,
}

impl GeminiAuth {
    /// Whether this credential is the Gemini CLI login.
    fn is_oauth(&self) -> bool {
        matches!(self, GeminiAuth::CliLogin)
    }

    /// The raw API key, for the variants that carry one.
    ///
    /// The Gemini CLI login carries no string: the login file is read when a
    /// request is sent, not kept here.
    fn api_key(&self) -> Option<&str> {
        match self {
            GeminiAuth::ExplicitKey(key)
            | GeminiAuth::EnvGeminiKey(key)
            | GeminiAuth::EnvGoogleKey(key) => Some(key),
            GeminiAuth::CliLogin => None,
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// API REQUEST/RESPONSE TYPES
// ══════════════════════════════════════════════════════════════════════════════

#[derive(Debug, Serialize, Clone)]
struct GenerateContentRequest {
    contents: Vec<Content>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

/// Request envelope for the internal cloudcode-pa API.
/// OAuth tokens from Gemini CLI are scoped for this endpoint.
#[derive(Debug, Serialize)]
struct InternalGenerateContentEnvelope {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_prompt_id: Option<String>,
    request: InternalGenerateContentRequest,
}

/// Nested request payload for cloudcode-pa's code assist APIs.
#[derive(Debug, Serialize)]
struct InternalGenerateContentRequest {
    contents: Vec<Content>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Debug, Serialize, Clone)]
struct Content {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<Part>,
}

#[derive(Debug, Serialize, Clone)]
struct Part {
    text: String,
}

#[derive(Debug, Serialize, Clone)]
struct GenerationConfig {
    temperature: f64,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct GenerateContentResponse {
    candidates: Option<Vec<Candidate>>,
    error: Option<ApiError>,
    #[serde(default)]
    response: Option<Box<GenerateContentResponse>>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    content: CandidateContent,
}

#[derive(Debug, Deserialize)]
struct CandidateContent {
    parts: Vec<ResponsePart>,
}

#[derive(Debug, Deserialize)]
struct ResponsePart {
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

impl GenerateContentResponse {
    /// cloudcode-pa wraps the actual response under `response`.
    fn into_effective_response(self) -> Self {
        match self {
            Self {
                response: Some(inner),
                ..
            } => *inner,
            other => other,
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// ENDPOINTS
// ══════════════════════════════════════════════════════════════════════════════

/// Internal API endpoint used by Gemini CLI for OAuth users.
/// See: https://github.com/google-gemini/gemini-cli/issues/19200
const CLOUDCODE_PA_ENDPOINT: &str = "https://cloudcode-pa.googleapis.com/v1internal";

/// Public API endpoint for API key users.
const PUBLIC_API_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";

impl GeminiProvider {
    /// Create a new Gemini provider.
    ///
    /// Authentication priority:
    /// 1. Explicit API key passed in
    /// 2. `GEMINI_API_KEY` environment variable
    /// 3. `GOOGLE_API_KEY` environment variable
    /// 4. Gemini CLI OAuth tokens (`~/.gemini/oauth_creds.json`)
    ///
    /// The CLI login is not read here: the login file is read when a request
    /// is sent, so a login renewed by running `gemini` is used on the next
    /// request, without a restart.
    pub fn new(api_key: Option<&str>) -> Self {
        let resolved_auth = api_key
            .and_then(Self::normalize_non_empty)
            .map(GeminiAuth::ExplicitKey)
            .or_else(|| Self::load_non_empty_env("GEMINI_API_KEY").map(GeminiAuth::EnvGeminiKey))
            .or_else(|| Self::load_non_empty_env("GOOGLE_API_KEY").map(GeminiAuth::EnvGoogleKey))
            .or_else(|| {
                // The login file holds a token — valid or expired. An expired
                // login still counts: the request that uses it is refused
                // with the fix named, which beats a key-less path that cannot
                // work either.
                (!matches!(
                    crate::providers::gemini_cli::read_gemini_cli_login(),
                    crate::providers::gemini_cli::Login::Missing
                ))
                .then_some(GeminiAuth::CliLogin)
            });

        Self {
            auth: resolved_auth,
        }
    }

    fn normalize_non_empty(value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn load_non_empty_env(name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .and_then(|value| Self::normalize_non_empty(&value))
    }

    /// Check if Gemini CLI is configured and has valid credentials
    pub fn has_cli_credentials() -> bool {
        crate::providers::gemini_cli::gemini_cli_has_credentials()
    }

    /// Check if any Gemini authentication is available
    pub fn has_any_auth() -> bool {
        Self::load_non_empty_env("GEMINI_API_KEY").is_some()
            || Self::load_non_empty_env("GOOGLE_API_KEY").is_some()
            || Self::has_cli_credentials()
    }

    /// Get authentication source description for diagnostics.
    /// Uses the stored enum variant — no env var re-reading at call time.
    pub fn auth_source(&self) -> &'static str {
        match self.auth.as_ref() {
            Some(GeminiAuth::ExplicitKey(_)) => "config",
            Some(GeminiAuth::EnvGeminiKey(_)) => "GEMINI_API_KEY env var",
            Some(GeminiAuth::EnvGoogleKey(_)) => "GOOGLE_API_KEY env var",
            Some(GeminiAuth::CliLogin) => "Gemini CLI OAuth",
            None => "none",
        }
    }

    fn format_model_name(model: &str) -> String {
        if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{model}")
        }
    }

    fn format_internal_model_name(model: &str) -> String {
        model.strip_prefix("models/").unwrap_or(model).to_string()
    }

    /// Build the API URL based on auth type.
    ///
    /// - API key users → public `generativelanguage.googleapis.com/v1beta`
    /// - OAuth users → internal `cloudcode-pa.googleapis.com/v1internal`
    ///
    /// The Gemini CLI OAuth tokens are scoped for the internal Code Assist API,
    /// not the public API. Sending them to the public endpoint results in
    /// "400 Bad Request: API key not valid" errors.
    /// See: https://github.com/google-gemini/gemini-cli/issues/19200
    fn build_generate_content_url(model: &str, auth: &GeminiAuth) -> String {
        match auth {
            GeminiAuth::CliLogin => {
                // OAuth tokens from Gemini CLI are scoped for the internal
                // Code Assist API. The model is passed in the request body,
                // not the URL path.
                format!("{CLOUDCODE_PA_ENDPOINT}:generateContent")
            }
            GeminiAuth::ExplicitKey(key)
            | GeminiAuth::EnvGeminiKey(key)
            | GeminiAuth::EnvGoogleKey(key) => {
                let model_name = Self::format_model_name(model);
                format!("{PUBLIC_API_ENDPOINT}/{model_name}:generateContent?key={key}")
            }
        }
    }

    fn http_client(&self) -> Client {
        crate::config::build_runtime_proxy_client_with_timeouts("provider.gemini", 120, 10)
    }

    /// Build the request for `auth`.
    ///
    /// The Gemini CLI login is read here, when the request is built: a login
    /// renewed by running `gemini` is used on the next request, and an
    /// expired one fails this request with the fix named — before any
    /// request leaves the machine.
    fn build_generate_content_request(
        &self,
        auth: &GeminiAuth,
        url: &str,
        request: &GenerateContentRequest,
        model: &str,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        match auth {
            GeminiAuth::CliLogin => {
                let token = Self::cli_login_token()?;
                // Internal Code Assist API uses a wrapped payload shape:
                // { model, project?, user_prompt_id?, request: { contents, systemInstruction?, generationConfig } }
                let internal_request = InternalGenerateContentEnvelope {
                    model: Self::format_internal_model_name(model),
                    project: Self::resolve_oauth_project_id(),
                    user_prompt_id: Some(uuid::Uuid::new_v4().to_string()),
                    request: InternalGenerateContentRequest {
                        contents: request.contents.clone(),
                        system_instruction: request.system_instruction.clone(),
                        generation_config: request.generation_config.clone(),
                    },
                };
                Ok(self
                    .http_client()
                    .post(url)
                    .json(&internal_request)
                    .bearer_auth(token))
            }
            GeminiAuth::ExplicitKey(_)
            | GeminiAuth::EnvGeminiKey(_)
            | GeminiAuth::EnvGoogleKey(_) => Ok(self.http_client().post(url).json(request)),
        }
    }

    /// The Gemini CLI login token, read now.
    ///
    /// An expired login fails the request with the refusal that names the
    /// fix, a missing one with the "no credential" message. Neither is sent
    /// to Google.
    fn cli_login_token() -> anyhow::Result<String> {
        match crate::providers::gemini_cli::read_gemini_cli_login() {
            crate::providers::gemini_cli::Login::Valid(token) => Ok(token),
            crate::providers::gemini_cli::Login::Expired => {
                anyhow::bail!(crate::providers::gemini_cli::EXPIRED_LOGIN_REFUSAL)
            }
            crate::providers::gemini_cli::Login::Missing => Err(Self::missing_credential_error()),
        }
    }

    /// The error a Gemini provider without a usable credential fails with.
    fn missing_credential_error() -> anyhow::Error {
        anyhow::anyhow!(
            "Gemini API key not found. Options:\n\
             1. Set GEMINI_API_KEY env var\n\
             2. Run `gemini` CLI to authenticate (tokens will be reused)\n\
             3. Get an API key from https://aistudio.google.com/app/apikey\n\
             4. Run `rantaiclaw onboard` to configure"
        )
    }

    fn resolve_oauth_project_id() -> Option<String> {
        for key in [
            "GEMINI_CODE_ASSIST_PROJECT",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_PROJECT_ID",
        ] {
            if let Ok(value) = std::env::var(key) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        None
    }
}

impl GeminiProvider {
    async fn send_generate_content(
        &self,
        contents: Vec<Content>,
        system_instruction: Option<Content>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let auth = self
            .auth
            .as_ref()
            .ok_or_else(Self::missing_credential_error)?;

        let request = GenerateContentRequest {
            contents,
            system_instruction,
            generation_config: GenerationConfig {
                temperature,
                max_output_tokens: 8192,
            },
        };

        let url = Self::build_generate_content_url(model, auth);

        let response = self
            .build_generate_content_request(auth, &url, &request, model)?
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API error ({status}): {error_text}");
        }

        let result: GenerateContentResponse = response.json().await?;
        if let Some(err) = &result.error {
            anyhow::bail!("Gemini API error: {}", err.message);
        }
        let result = result.into_effective_response();
        if let Some(err) = result.error {
            anyhow::bail!("Gemini API error: {}", err.message);
        }

        result
            .candidates
            .and_then(|c| c.into_iter().next())
            .and_then(|c| c.content.parts.into_iter().next())
            .and_then(|p| p.text)
            .ok_or_else(|| anyhow::anyhow!("No response from Gemini"))
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let system_instruction = system_prompt.map(|sys| Content {
            role: None,
            parts: vec![Part {
                text: sys.to_string(),
            }],
        });

        let contents = vec![Content {
            role: Some("user".to_string()),
            parts: vec![Part {
                text: message.to_string(),
            }],
        }];

        self.send_generate_content(contents, system_instruction, model, temperature)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let mut system_parts: Vec<&str> = Vec::new();
        let mut contents: Vec<Content> = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" => {
                    system_parts.push(&msg.content);
                }
                "user" => {
                    contents.push(Content {
                        role: Some("user".to_string()),
                        parts: vec![Part {
                            text: msg.content.clone(),
                        }],
                    });
                }
                "assistant" => {
                    // Gemini API uses "model" role instead of "assistant"
                    contents.push(Content {
                        role: Some("model".to_string()),
                        parts: vec![Part {
                            text: msg.content.clone(),
                        }],
                    });
                }
                _ => {}
            }
        }

        let system_instruction = if system_parts.is_empty() {
            None
        } else {
            Some(Content {
                role: None,
                parts: vec![Part {
                    text: system_parts.join("\n\n"),
                }],
            })
        };

        self.send_generate_content(contents, system_instruction, model, temperature)
            .await
    }

    async fn warmup(&self) -> anyhow::Result<()> {
        if let Some(auth) = self.auth.as_ref() {
            // cloudcode-pa does not expose a lightweight model-list probe like the public API.
            // Avoid false negatives for valid Gemini CLI OAuth credentials.
            if auth.is_oauth() {
                return Ok(());
            }

            let url = match auth.api_key() {
                Some(key) => {
                    format!("https://generativelanguage.googleapis.com/v1beta/models?key={key}")
                }
                None => "https://generativelanguage.googleapis.com/v1beta/models".to_string(),
            };

            self.http_client()
                .get(&url)
                .send()
                .await?
                .error_for_status()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::gemini_cli::{
        gemini_creds_path, gemini_home, write_gemini_creds_with_expiry, EXPIRED_LOGIN_REFUSAL,
    };
    use reqwest::header::AUTHORIZATION;

    /// The request body the provider builds for one user turn.
    fn user_request() -> GenerateContentRequest {
        GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".into()),
                parts: vec![Part {
                    text: "hello".into(),
                }],
            }],
            system_instruction: None,
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
        }
    }

    /// The credential the provider resolved when it was built.
    fn cli_login_auth(provider: &GeminiProvider) -> &GeminiAuth {
        provider
            .auth
            .as_ref()
            .expect("the provider uses the Gemini CLI login")
    }

    #[test]
    fn normalize_non_empty_trims_and_filters() {
        assert_eq!(
            GeminiProvider::normalize_non_empty(" value "),
            Some("value".into())
        );
        assert_eq!(GeminiProvider::normalize_non_empty(""), None);
        assert_eq!(GeminiProvider::normalize_non_empty(" \t\n"), None);
    }

    #[test]
    fn provider_creates_without_key() {
        let provider = GeminiProvider::new(None);
        // May pick up env vars; just verify it doesn't panic
        let _ = provider.auth_source();
    }

    #[test]
    fn provider_creates_with_key() {
        let provider = GeminiProvider::new(Some("test-api-key"));
        assert!(matches!(
            provider.auth,
            Some(GeminiAuth::ExplicitKey(ref key)) if key == "test-api-key"
        ));
    }

    #[test]
    fn provider_rejects_empty_key() {
        let provider = GeminiProvider::new(Some(""));
        assert!(!matches!(provider.auth, Some(GeminiAuth::ExplicitKey(_))));
    }

    #[test]
    fn auth_source_explicit_key() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::ExplicitKey("key".into())),
        };
        assert_eq!(provider.auth_source(), "config");
    }

    #[test]
    fn auth_source_none_without_credentials() {
        let provider = GeminiProvider { auth: None };
        assert_eq!(provider.auth_source(), "none");
    }

    #[test]
    fn auth_source_oauth() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::CliLogin),
        };
        assert_eq!(provider.auth_source(), "Gemini CLI OAuth");
    }

    /// A key wins over the CLI login: an explicit key first, then
    /// `GEMINI_API_KEY`, then `GOOGLE_API_KEY`.
    #[tokio::test]
    async fn a_key_wins_over_the_cli_login() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let (home, _home_guard) = gemini_home();

        write_gemini_creds_with_expiry(&home, "ya29.mock-token", 3_600_000);

        let provider = GeminiProvider::new(Some("explicit-key"));
        assert_eq!(provider.auth_source(), "config");

        let _gemini_key = crate::test_env::EnvGuard::set("GEMINI_API_KEY", "env-key");
        let provider = GeminiProvider::new(None);
        assert_eq!(provider.auth_source(), "GEMINI_API_KEY env var");

        let _gemini_key = crate::test_env::EnvGuard::unset("GEMINI_API_KEY");
        let _google_key = crate::test_env::EnvGuard::set("GOOGLE_API_KEY", "google-key");
        let provider = GeminiProvider::new(None);
        assert_eq!(provider.auth_source(), "GOOGLE_API_KEY env var");
    }

    #[test]
    fn model_name_formatting() {
        assert_eq!(
            GeminiProvider::format_model_name("gemini-2.0-flash"),
            "models/gemini-2.0-flash"
        );
        assert_eq!(
            GeminiProvider::format_model_name("models/gemini-1.5-pro"),
            "models/gemini-1.5-pro"
        );
        assert_eq!(
            GeminiProvider::format_internal_model_name("models/gemini-2.5-flash"),
            "gemini-2.5-flash"
        );
        assert_eq!(
            GeminiProvider::format_internal_model_name("gemini-2.5-flash"),
            "gemini-2.5-flash"
        );
    }

    #[test]
    fn api_key_url_includes_key_query_param() {
        let auth = GeminiAuth::ExplicitKey("api-key-123".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        assert!(url.contains(":generateContent?key=api-key-123"));
    }

    #[test]
    fn oauth_url_uses_internal_endpoint() {
        let auth = GeminiAuth::CliLogin;
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        assert!(url.starts_with("https://cloudcode-pa.googleapis.com/v1internal"));
        assert!(url.ends_with(":generateContent"));
        assert!(!url.contains("generativelanguage.googleapis.com"));
        assert!(!url.contains("?key="));
    }

    #[test]
    fn api_key_url_uses_public_endpoint() {
        let auth = GeminiAuth::ExplicitKey("api-key-123".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        assert!(url.contains("generativelanguage.googleapis.com/v1beta"));
        assert!(url.contains("models/gemini-2.0-flash"));
    }

    /// The Gemini CLI login is sent as a bearer token. The provider is built
    /// through `new()` and reads the login file, like the request path does.
    #[tokio::test]
    async fn oauth_request_uses_bearer_auth_header() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let (home, _home_guard) = gemini_home();

        write_gemini_creds_with_expiry(&home, "ya29.mock-token", 3_600_000);
        let provider = GeminiProvider::new(None);

        let auth = cli_login_auth(&provider);
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", auth);
        let request = provider
            .build_generate_content_request(auth, &url, &user_request(), "gemini-2.0-flash")
            .expect("a valid login builds a request")
            .build()
            .expect("the request builds");

        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|h| h.to_str().ok()),
            Some("Bearer ya29.mock-token")
        );
    }

    /// A login renewed by running `gemini` is used on the next request: the
    /// provider was built with one token and sends the other one.
    #[tokio::test]
    async fn a_renewed_cli_login_is_used_on_the_next_request() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let (home, _home_guard) = gemini_home();

        write_gemini_creds_with_expiry(&home, "ya29.mock-token-a", 3_600_000);
        let provider = GeminiProvider::new(None);

        // The login is renewed with another token.
        write_gemini_creds_with_expiry(&home, "ya29.mock-token-b", 3_600_000);

        let auth = cli_login_auth(&provider);
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", auth);
        let request = provider
            .build_generate_content_request(auth, &url, &user_request(), "gemini-2.0-flash")
            .expect("a renewed login builds a request")
            .build()
            .expect("the request builds");

        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|h| h.to_str().ok()),
            Some("Bearer ya29.mock-token-b")
        );
    }

    /// A login that died after the provider was built fails the next
    /// request with the fix named, and no request is built.
    #[tokio::test]
    async fn an_expired_cli_login_refuses_the_next_request() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let (home, _home_guard) = gemini_home();

        write_gemini_creds_with_expiry(&home, "ya29.mock-token", 3_600_000);
        let provider = GeminiProvider::new(None);
        assert_eq!(provider.auth_source(), "Gemini CLI OAuth");

        // The login dies.
        write_gemini_creds_with_expiry(&home, "ya29.mock-token", -3_600_000);

        let error = provider
            .chat_with_system(None, "hello", "gemini-2.0-flash", 0.7)
            .await
            .expect_err("an expired login must fail the request");
        assert!(error.to_string().contains(EXPIRED_LOGIN_REFUSAL), "{error}");

        // No request is built at all: the login is refused before the
        // request exists.
        let auth = cli_login_auth(&provider);
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", auth);
        assert!(
            provider
                .build_generate_content_request(auth, &url, &user_request(), "gemini-2.0-flash")
                .is_err(),
            "an expired login must not build a request"
        );
    }

    /// The login file is gone when the request is sent: the request is
    /// refused with the "no credential" message.
    #[tokio::test]
    async fn a_deleted_cli_login_file_refuses_the_next_request() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let (home, _home_guard) = gemini_home();

        write_gemini_creds_with_expiry(&home, "ya29.mock-token", 3_600_000);
        let provider = GeminiProvider::new(None);

        std::fs::remove_file(gemini_creds_path(&home)).expect("remove creds");

        let error = provider
            .chat_with_system(None, "hello", "gemini-2.0-flash", 0.7)
            .await
            .expect_err("a missing login must fail the request");
        assert!(
            error.to_string().contains("Gemini API key not found"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn oauth_request_wraps_payload_in_request_envelope() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let (home, _home_guard) = gemini_home();

        write_gemini_creds_with_expiry(&home, "ya29.mock-token", 3_600_000);
        let provider = GeminiProvider::new(None);

        let auth = cli_login_auth(&provider);
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", auth);
        let body = user_request();

        let request = provider
            .build_generate_content_request(auth, &url, &body, "models/gemini-2.0-flash")
            .expect("a valid login builds a request")
            .build()
            .expect("the request builds");

        let payload = request
            .body()
            .and_then(|b| b.as_bytes())
            .expect("json request body should be bytes");
        let json: serde_json::Value = serde_json::from_slice(payload).unwrap();

        assert_eq!(json["model"], "gemini-2.0-flash");
        assert!(json.get("generationConfig").is_none());
        assert!(json.get("request").is_some());
        assert!(json["request"].get("generationConfig").is_some());
    }

    #[test]
    fn api_key_request_does_not_set_bearer_header() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::ExplicitKey("api-key-123".into())),
        };
        let auth = GeminiAuth::ExplicitKey("api-key-123".into());
        let url = GeminiProvider::build_generate_content_url("gemini-2.0-flash", &auth);
        let body = user_request();

        let request = provider
            .build_generate_content_request(&auth, &url, &body, "gemini-2.0-flash")
            .expect("an API key builds a request")
            .build()
            .unwrap();

        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    #[test]
    fn request_serialization() {
        let request = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".to_string()),
                parts: vec![Part {
                    text: "Hello".to_string(),
                }],
            }],
            system_instruction: Some(Content {
                role: None,
                parts: vec![Part {
                    text: "You are helpful".to_string(),
                }],
            }),
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"text\":\"Hello\""));
        assert!(json.contains("\"systemInstruction\""));
        assert!(!json.contains("\"system_instruction\""));
        assert!(json.contains("\"temperature\":0.7"));
        assert!(json.contains("\"maxOutputTokens\":8192"));
    }

    #[test]
    fn internal_request_includes_model() {
        let request = InternalGenerateContentEnvelope {
            model: "gemini-3-pro-preview".to_string(),
            project: None,
            user_prompt_id: Some("prompt-123".to_string()),
            request: InternalGenerateContentRequest {
                contents: vec![Content {
                    role: Some("user".to_string()),
                    parts: vec![Part {
                        text: "Hello".to_string(),
                    }],
                }],
                system_instruction: None,
                generation_config: GenerationConfig {
                    temperature: 0.7,
                    max_output_tokens: 8192,
                },
            },
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"model\":\"gemini-3-pro-preview\""));
        assert!(json.contains("\"request\""));
        assert!(json.contains("\"generationConfig\""));
        assert!(json.contains("\"maxOutputTokens\":8192"));
        assert!(json.contains("\"user_prompt_id\":\"prompt-123\""));
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"temperature\":0.7"));
    }

    #[test]
    fn response_deserialization() {
        let json = r#"{
            "candidates": [{
                "content": {
                    "parts": [{"text": "Hello there!"}]
                }
            }]
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        assert!(response.candidates.is_some());
        let text = response
            .candidates
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .content
            .parts
            .into_iter()
            .next()
            .unwrap()
            .text;
        assert_eq!(text, Some("Hello there!".to_string()));
    }

    #[test]
    fn error_response_deserialization() {
        let json = r#"{
            "error": {
                "message": "Invalid API key"
            }
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().message, "Invalid API key");
    }

    #[test]
    fn internal_response_deserialization() {
        let json = r#"{
            "response": {
                "candidates": [{
                    "content": {
                        "parts": [{"text": "Hello from internal"}]
                    }
                }]
            }
        }"#;

        let response: GenerateContentResponse = serde_json::from_str(json).unwrap();
        let text = response
            .into_effective_response()
            .candidates
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .content
            .parts
            .into_iter()
            .next()
            .unwrap()
            .text;
        assert_eq!(text, Some("Hello from internal".to_string()));
    }

    #[tokio::test]
    async fn warmup_without_key_is_noop() {
        let provider = GeminiProvider { auth: None };
        let result = provider.warmup().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn warmup_oauth_is_noop() {
        let provider = GeminiProvider {
            auth: Some(GeminiAuth::CliLogin),
        };
        let result = provider.warmup().await;
        assert!(result.is_ok());
    }
}
