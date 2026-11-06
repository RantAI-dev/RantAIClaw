//! Google Gemini provider, API key only.
//!
//! The credential is an API key: the explicit config value, `GEMINI_API_KEY`
//! or `GOOGLE_API_KEY`. The Gemini CLI login is gone — RantaiClaw uses API
//! keys only — so a provider without a key has nothing to send and refuses
//! the request, with the key named.

use crate::providers::gemini_error::GeminiAuthError;
use crate::providers::traits::{ChatMessage, Provider};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

/// What to say when the only Gemini credential is missing.
///
/// Names the API key and where to get one. The CLI login is not an option:
/// it was removed.
pub(crate) const MISSING_CREDENTIAL_REFUSAL: &str = "Gemini API key not found. Options:\n\
     1. Set GEMINI_API_KEY env var\n\
     2. Get an API key from https://aistudio.google.com/app/apikey\n\
     3. Run `rantaiclaw onboard` to configure";

/// Gemini provider, authenticated with an API key.
pub struct GeminiProvider {
    auth: GeminiAuth,
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
    /// No API key at all: every request is refused.
    Missing,
}

impl GeminiAuth {
    /// The raw API key, for the variants that carry one.
    fn api_key(&self) -> Option<&str> {
        match self {
            GeminiAuth::ExplicitKey(key)
            | GeminiAuth::EnvGeminiKey(key)
            | GeminiAuth::EnvGoogleKey(key) => Some(key),
            GeminiAuth::Missing => None,
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

// ══════════════════════════════════════════════════════════════════════════════
// ENDPOINTS
// ══════════════════════════════════════════════════════════════════════════════

/// Public API endpoint for API key users.
const PUBLIC_API_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";

impl GeminiProvider {
    /// Create a new Gemini provider.
    ///
    /// Authentication priority:
    /// 1. Explicit API key passed in
    /// 2. `GEMINI_API_KEY` environment variable
    /// 3. `GOOGLE_API_KEY` environment variable
    ///
    /// Without a key the provider still builds: the request is refused when it
    /// is sent, so the gateway binds and the daemon stays up.
    pub fn new(api_key: Option<&str>) -> Self {
        let resolved_auth = api_key
            .and_then(Self::normalize_non_empty)
            .map(GeminiAuth::ExplicitKey)
            .or_else(|| Self::load_non_empty_env("GEMINI_API_KEY").map(GeminiAuth::EnvGeminiKey))
            .or_else(|| Self::load_non_empty_env("GOOGLE_API_KEY").map(GeminiAuth::EnvGoogleKey))
            .unwrap_or(GeminiAuth::Missing);

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

    /// Get authentication source description for diagnostics.
    /// Uses the stored enum variant — no env var re-reading at call time.
    pub fn auth_source(&self) -> &'static str {
        match &self.auth {
            GeminiAuth::ExplicitKey(_) => "config",
            GeminiAuth::EnvGeminiKey(_) => "GEMINI_API_KEY env var",
            GeminiAuth::EnvGoogleKey(_) => "GOOGLE_API_KEY env var",
            GeminiAuth::Missing => "no API key",
        }
    }

    fn format_model_name(model: &str) -> String {
        if model.starts_with("models/") {
            model.to_string()
        } else {
            format!("models/{model}")
        }
    }

    /// The URL the request goes to, for the provider's key.
    ///
    /// `None` when the provider has no key: the request is refused instead,
    /// before anything is sent.
    fn generate_content_url(&self, model: &str) -> Option<String> {
        let key = self.auth.api_key()?;
        let model_name = Self::format_model_name(model);
        Some(format!(
            "{PUBLIC_API_ENDPOINT}/{model_name}:generateContent?key={key}"
        ))
    }

    fn http_client(&self) -> Client {
        crate::config::build_runtime_proxy_client_with_timeouts("provider.gemini", 120, 10)
    }

    /// Build the request for the provider's credential.
    ///
    /// Without an API key the request is refused here: nothing is built and
    /// nothing leaves the machine.
    fn build_generate_content_request(
        &self,
        url: &str,
        request: &GenerateContentRequest,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        if self.auth.api_key().is_none() {
            return Err(Self::missing_credential_error());
        }
        Ok(self.http_client().post(url).json(request))
    }

    /// Build the request a single user turn would produce, without sending it.
    ///
    /// The seam for "what would go on the wire": a provider without a key
    /// fails here and builds nothing. Building a request does not send it, so
    /// a test that uses this seam cannot reach Google.
    #[cfg(test)]
    pub(crate) fn build_request_for_test(&self, model: &str) -> anyhow::Result<reqwest::Request> {
        let request = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".to_string()),
                parts: vec![Part {
                    text: "hello".to_string(),
                }],
            }],
            system_instruction: None,
            generation_config: GenerationConfig {
                temperature: 0.7,
                max_output_tokens: 8192,
            },
        };
        let url = self
            .generate_content_url(model)
            .ok_or_else(Self::missing_credential_error)?;
        self.build_generate_content_request(&url, &request)?
            .build()
            .map_err(anyhow::Error::from)
    }

    /// The error a Gemini provider without a usable credential fails with.
    fn missing_credential_error() -> anyhow::Error {
        GeminiAuthError::MissingCredential.into()
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
        let request = GenerateContentRequest {
            contents,
            system_instruction,
            generation_config: GenerationConfig {
                temperature,
                max_output_tokens: 8192,
            },
        };

        // No key: the request is refused before anything is sent.
        let url = self
            .generate_content_url(model)
            .ok_or_else(Self::missing_credential_error)?;

        let response = self
            .build_generate_content_request(&url, &request)?
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
    /// The provider itself: the factory builds this type, so a test that
    /// reaches it through the wrappers has reached the factory's build.
    #[cfg(test)]
    fn test_as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

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
        // Without a key there is nothing to probe: the request would be
        // refused anyway, and a warmup must not fail the startup for it.
        let Some(key) = self.auth.api_key() else {
            return Ok(());
        };
        let url = format!("https://generativelanguage.googleapis.com/v1beta/models?key={key}");

        self.http_client()
            .get(&url)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            GeminiAuth::ExplicitKey(ref key) if key == "test-api-key"
        ));
    }

    #[test]
    fn provider_rejects_empty_key() {
        let provider = GeminiProvider::new(Some(""));
        assert!(!matches!(provider.auth, GeminiAuth::ExplicitKey(_)));
    }

    #[test]
    fn auth_source_explicit_key() {
        let provider = GeminiProvider {
            auth: GeminiAuth::ExplicitKey("key".into()),
        };
        assert_eq!(provider.auth_source(), "config");
    }

    /// Without a key the provider has no credential at all: the request is
    /// refused, and the fix is named.
    #[tokio::test]
    async fn a_provider_without_a_key_refuses_the_request_with_the_fix_named() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let provider = GeminiProvider::new(None);
        assert_eq!(provider.auth_source(), "no API key");

        let error = provider
            .build_request_for_test("gemini-2.0-flash")
            .expect_err("a provider without a key must fail the request");
        let message = error.to_string();
        assert!(message.contains("Gemini API key not found"), "{message}");
        assert!(message.contains("GEMINI_API_KEY"), "{message}");
    }

    /// The Gemini CLI login is gone. A `~/.gemini/oauth_creds.json` on disk
    /// must not be read, and a provider without a key must still refuse.
    ///
    /// The file holds a marker that would show up in the request if it were
    /// read, so the refusal proves the file was never opened. `HOME` points at
    /// a temp dir, so the operator's own file cannot be the one that is read.
    #[tokio::test]
    async fn a_gemini_oauth_creds_file_is_not_read() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();
        let home = tempfile::tempdir().expect("temp home");
        let gemini_dir = home.path().join(".gemini");
        std::fs::create_dir_all(&gemini_dir).expect("gemini dir");
        std::fs::write(
            gemini_dir.join("oauth_creds.json"),
            r#"{"access_token":"marker-from-the-cli-login"}"#,
        )
        .expect("marker file");
        let _restore = crate::test_env::HomeGuard::set(home.path());

        let provider = GeminiProvider::new(None);
        assert_eq!(
            provider.auth_source(),
            "no API key",
            "the CLI login file must not stand in for an API key"
        );

        let error = provider
            .build_request_for_test("gemini-2.0-flash")
            .expect_err("a provider without a key must fail the request");
        let message = error.to_string();
        assert!(message.contains("GEMINI_API_KEY"), "{message}");
    }

    /// The `gemini` key path: the configured API key is the credential that
    /// goes out — as the key query parameter, never as a login token.
    ///
    /// The assertion is on the request that WOULD be built: a test must never
    /// send anything to Google, not even a request the provider refuses.
    #[tokio::test]
    async fn the_configured_gemini_key_is_the_credential_that_goes_out() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let provider = GeminiProvider::new(Some("configured-gemini-key"));
        assert_eq!(provider.auth_source(), "config");

        let request = provider
            .build_request_for_test("gemini-2.0-flash")
            .expect("a configured key builds a request");
        let url = request.url().to_string();
        assert!(
            url.contains("key=configured-gemini-key"),
            "the configured key must be the credential that goes out: {url}"
        );
        assert!(
            request.headers().get(AUTHORIZATION).is_none(),
            "the key must not travel as a bearer token: {url}"
        );
    }

    /// A key wins over the environment: an explicit key first, then
    /// `GEMINI_API_KEY`, then `GOOGLE_API_KEY`.
    #[tokio::test]
    async fn a_key_wins_over_the_environment() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        // The env key is present for the whole test: an explicit key must
        // still win, or the key the operator configured is not the one sent.
        let _gemini_key = crate::test_env::EnvGuard::set("GEMINI_API_KEY", "env-key");

        let provider = GeminiProvider::new(Some("explicit-key"));
        assert_eq!(provider.auth_source(), "config");

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
    }

    #[test]
    fn api_key_url_includes_key_query_param() {
        let provider = GeminiProvider {
            auth: GeminiAuth::ExplicitKey("api-key-123".into()),
        };
        let url = provider
            .generate_content_url("gemini-2.0-flash")
            .expect("a key builds a url");
        assert!(url.contains(":generateContent?key=api-key-123"));
    }

    #[test]
    fn api_key_url_uses_public_endpoint() {
        let provider = GeminiProvider {
            auth: GeminiAuth::ExplicitKey("api-key-123".into()),
        };
        let url = provider
            .generate_content_url("gemini-2.0-flash")
            .expect("a key builds a url");
        assert!(url.contains("generativelanguage.googleapis.com/v1beta"));
        assert!(url.contains("models/gemini-2.0-flash"));
    }

    /// The key travels in the query string, never as a bearer token.
    #[test]
    fn api_key_request_does_not_set_bearer_header() {
        let provider = GeminiProvider {
            auth: GeminiAuth::ExplicitKey("api-key-123".into()),
        };
        let url = provider
            .generate_content_url("gemini-2.0-flash")
            .expect("a key builds a url");
        let body = user_request();

        let request = provider
            .build_generate_content_request(&url, &body)
            .expect("an API key builds a request")
            .build()
            .unwrap();

        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    #[test]
    fn request_serialization() {
        let request = GenerateContentRequest {
            contents: vec![Content {
                role: Some("user".into()),
                parts: vec![Part {
                    text: "Hello".into(),
                }],
            }],
            system_instruction: Some(Content {
                role: None,
                parts: vec![Part {
                    text: "You are helpful".into(),
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

    /// A provider without a key has nothing to probe, and a warmup must not
    /// fail the startup for it.
    #[tokio::test]
    async fn warmup_without_a_key_is_a_noop() {
        let provider = GeminiProvider {
            auth: GeminiAuth::Missing,
        };
        let result = provider.warmup().await;
        assert!(result.is_ok());
    }
}
