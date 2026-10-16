use super::browser::{
    assert_host_resolves_to_public, assert_resolved_addrs_public, check_url_host,
    normalize_allowed_domains,
};
use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Open approved HTTPS URLs in Brave Browser (no scraping, no DOM automation).
pub struct BrowserOpenTool {
    security: Arc<SecurityPolicy>,
    allowed_domains: Vec<String>,
}

impl BrowserOpenTool {
    pub fn new(security: Arc<SecurityPolicy>, allowed_domains: Vec<String>) -> Self {
        Self {
            security,
            allowed_domains: normalize_allowed_domains(allowed_domains),
        }
    }

    fn validate_url(&self, raw_url: &str) -> anyhow::Result<String> {
        let url = raw_url.trim();

        if url.is_empty() {
            anyhow::bail!("URL cannot be empty");
        }

        if url.chars().any(char::is_whitespace) {
            anyhow::bail!("URL cannot contain whitespace");
        }

        if !url.starts_with("https://") {
            anyhow::bail!("Only https:// URLs are allowed");
        }

        check_url_host(url, &self.allowed_domains)?;

        Ok(url.to_string())
    }

    /// Everything that must hold before Brave is launched: the literal host check,
    /// then the resolved-address check. Resolution runs through the same helper
    /// `browser` uses, so a public name that points at a private address is refused.
    async fn assert_open_allowed(&self, raw_url: &str) -> anyhow::Result<String> {
        self.assert_open_allowed_with(raw_url, None).await
    }

    /// `addrs` is the test seam: `Some` supplies the resolved addresses so the
    /// decision is testable without DNS; `None` performs the real lookup.
    async fn assert_open_allowed_with(
        &self,
        raw_url: &str,
        addrs: Option<&[std::net::SocketAddr]>,
    ) -> anyhow::Result<String> {
        let url = self.validate_url(raw_url)?;
        match addrs {
            Some(addrs) => assert_resolved_addrs_public(&url, addrs)?,
            None => assert_host_resolves_to_public(&url).await?,
        }
        Ok(url)
    }
}

#[async_trait]
impl Tool for BrowserOpenTool {
    fn name(&self) -> &str {
        "browser_open"
    }

    fn description(&self) -> &str {
        "Open an approved HTTPS URL in Brave Browser. Security constraints: allowlist-only domains, no local/private hosts, no scraping."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "HTTPS URL to open in Brave Browser"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'url' parameter"))?;

        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: rate limit exceeded".into()),
            });
        }

        let url = match self.assert_open_allowed(url).await {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                })
            }
        };

        match open_in_brave(&url).await {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("Opened in Brave: {url}"),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to open Brave Browser: {e}")),
            }),
        }
    }
}

async fn open_in_brave(url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        for app in ["Brave Browser", "Brave"] {
            let status = tokio::process::Command::new("open")
                .arg("-a")
                .arg(app)
                .arg(url)
                .status()
                .await;

            if let Ok(s) = status {
                if s.success() {
                    return Ok(());
                }
            }
        }
        anyhow::bail!(
            "Brave Browser was not found (tried macOS app names 'Brave Browser' and 'Brave')"
        );
    }

    #[cfg(target_os = "linux")]
    {
        let mut last_error = String::new();
        for cmd in ["brave-browser", "brave"] {
            match tokio::process::Command::new(cmd).arg(url).status().await {
                Ok(status) if status.success() => return Ok(()),
                Ok(status) => {
                    last_error = format!("{cmd} exited with status {status}");
                }
                Err(e) => {
                    last_error = format!("{cmd} not runnable: {e}");
                }
            }
        }
        anyhow::bail!("{last_error}");
    }

    #[cfg(target_os = "windows")]
    {
        let status = tokio::process::Command::new("cmd")
            .args(["/C", "start", "", "brave", url])
            .status()
            .await?;

        if status.success() {
            return Ok(());
        }

        anyhow::bail!("cmd start brave exited with status {status}");
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = url;
        anyhow::bail!("browser_open is not supported on this OS");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use crate::tools::browser::BrowserTool;

    fn test_tool(allowed_domains: Vec<&str>) -> BrowserOpenTool {
        let security = Arc::new(SecurityPolicy::default().with_autonomy(AutonomyLevel::Supervised));
        BrowserOpenTool::new(
            security,
            allowed_domains.into_iter().map(String::from).collect(),
        )
    }

    #[test]
    fn validate_accepts_exact_domain() {
        let tool = test_tool(vec!["example.com"]);
        let got = tool.validate_url("https://example.com/docs").unwrap();
        assert_eq!(got, "https://example.com/docs");
    }

    #[test]
    fn validate_accepts_subdomain() {
        let tool = test_tool(vec!["example.com"]);
        assert!(tool.validate_url("https://api.example.com/v1").is_ok());
    }

    #[test]
    fn validate_rejects_http() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool
            .validate_url("http://example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("https://"));
    }

    #[test]
    fn validate_rejects_localhost() {
        let tool = test_tool(vec!["localhost"]);
        let err = tool
            .validate_url("https://localhost:8080")
            .unwrap_err()
            .to_string();
        assert!(err.contains("local/private"));
    }

    #[test]
    fn validate_rejects_private_ipv4() {
        let tool = test_tool(vec!["192.168.1.5"]);
        let err = tool
            .validate_url("https://192.168.1.5")
            .unwrap_err()
            .to_string();
        assert!(err.contains("local/private"));
    }

    #[test]
    fn validate_rejects_allowlist_miss() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool
            .validate_url("https://google.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed_domains"));
    }

    #[test]
    fn validate_rejects_whitespace() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool
            .validate_url("https://example.com/hello world")
            .unwrap_err()
            .to_string();
        assert!(err.contains("whitespace"));
    }

    #[test]
    fn validate_rejects_userinfo() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool
            .validate_url("https://user@example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("userinfo"));
    }

    #[test]
    fn validate_requires_allowlist() {
        let security = Arc::new(SecurityPolicy::default());
        let tool = BrowserOpenTool::new(security, vec![]);
        let err = tool
            .validate_url("https://example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed_domains"));
    }

    /// Run the same URL through both browser tools and return each verdict as
    /// `Ok(())` or the refusal text. The two tools share `[browser].allowed_domains`.
    fn verdicts(allowed: Vec<String>, url: &str) -> (Result<(), String>, Result<(), String>) {
        let security = Arc::new(SecurityPolicy::default().with_autonomy(AutonomyLevel::Supervised));
        let open = BrowserOpenTool::new(security.clone(), allowed.clone());
        let browser = BrowserTool::new(security, allowed, None);
        (
            open.validate_url(url)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            browser.validate_url(url).map_err(|e| e.to_string()),
        )
    }

    const PRIVATE_OR_LOCAL_URLS: [&str; 5] = [
        "https://127.0.0.1",
        "https://10.0.0.1",
        "https://[::1]",
        "https://foo.local",
        "https://localhost",
    ];

    #[test]
    fn fresh_default_accepts_a_public_host_in_both_tools() {
        let (open, browser) = verdicts(
            crate::config::BrowserConfig::default().allowed_domains,
            "https://example.com",
        );
        assert!(open.is_ok(), "browser_open refused: {open:?}");
        assert!(browser.is_ok(), "browser refused: {browser:?}");
    }

    #[test]
    fn fresh_default_refuses_private_and_local_hosts_with_one_message() {
        for url in PRIVATE_OR_LOCAL_URLS {
            let (open, browser) =
                verdicts(crate::config::BrowserConfig::default().allowed_domains, url);
            assert!(open.is_err(), "browser_open accepted {url}");
            assert_eq!(open, browser, "tools disagree on {url}");
            assert!(
                open.unwrap_err().starts_with("Blocked local/private host"),
                "{url} must be refused as local/private"
            );
        }
    }

    #[test]
    fn explicit_empty_allowlist_gets_one_message_from_both_tools() {
        let (open, browser) = verdicts(vec![], "https://example.com");
        assert!(open.is_err() && browser.is_err());
        assert_eq!(open, browser);
        assert!(open.unwrap_err().contains("allowed_domains"));
    }

    #[test]
    fn wildcard_subdomain_allowlist_gives_one_verdict_in_both_tools() {
        let allowed = vec!["*.example.com".to_string()];

        let (open, browser) = verdicts(allowed.clone(), "https://a.example.com/x");
        assert!(open.is_ok(), "browser_open refused a.example.com: {open:?}");
        assert!(
            browser.is_ok(),
            "browser refused a.example.com: {browser:?}"
        );

        let (open, browser) = verdicts(allowed, "https://example.org");
        assert!(open.is_err() && browser.is_err());
        assert_eq!(open, browser);
    }

    #[test]
    fn fresh_default_refuses_every_numeric_ipv4_spelling_in_both_tools() {
        for url in [
            "https://127.0.0.1",
            "https://127.1",
            "https://2130706433",
            "https://0x7f000001",
            "https://0177.0.0.1",
        ] {
            let (open, browser) =
                verdicts(crate::config::BrowserConfig::default().allowed_domains, url);
            assert!(open.is_err(), "browser_open accepted {url}");
            assert_eq!(open, browser, "tools disagree on {url}");
            assert!(
                open.unwrap_err().starts_with("Blocked local/private host"),
                "{url} must be refused as local/private"
            );
        }
    }

    #[test]
    fn literal_check_still_passes_public_hosts() {
        let tool = test_tool(vec!["*"]);
        assert!(tool.validate_url("https://example.com").is_ok());
        assert!(tool.validate_url("https://93.184.215.14").is_ok());
    }

    #[tokio::test]
    async fn open_guard_refuses_when_resolved_address_is_private() {
        let tool = test_tool(vec!["*"]);
        let private: Vec<std::net::SocketAddr> = vec!["127.0.0.1:443".parse().unwrap()];
        let err = tool
            .assert_open_allowed_with("https://rebind.example/x", Some(&private))
            .await
            .expect_err("a public name resolving to loopback must be refused")
            .to_string();
        assert!(err.contains("private/local"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn open_guard_allows_when_all_resolved_addresses_are_public() {
        let tool = test_tool(vec!["*"]);
        let public: Vec<std::net::SocketAddr> = vec!["93.184.215.14:443".parse().unwrap()];
        let url = tool
            .assert_open_allowed_with("https://example.com/x", Some(&public))
            .await
            .expect("an all-public resolution must pass the guard");
        assert_eq!(url, "https://example.com/x");
    }

    #[tokio::test]
    async fn execute_blocks_readonly_mode() {
        let security = Arc::new(SecurityPolicy::default().with_autonomy(AutonomyLevel::ReadOnly));
        let tool = BrowserOpenTool::new(security, vec!["example.com".into()]);
        let result = tool
            .execute(json!({"url": "https://example.com"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn execute_blocks_when_rate_limited() {
        let security = Arc::new(SecurityPolicy::default().with_max_actions_per_hour(0));
        let tool = BrowserOpenTool::new(security, vec!["example.com".into()]);
        let result = tool
            .execute(json!({"url": "https://example.com"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[tokio::test]
    async fn open_guard_refuses_a_host_that_cannot_resolve() {
        // `.invalid` is reserved by RFC 6761 and never resolves, so this
        // exercises the production `None` arm (real DNS lookup) without
        // depending on a particular network failure path: with or without
        // DNS available, the lookup cannot return a public address.
        let tool = test_tool(vec!["*"]);
        let err = tool
            .assert_open_allowed("https://rantaiclaw.invalid/")
            .await
            .expect_err(".invalid host must be refused by the resolve guard")
            .to_string();
        assert!(
            err.contains("Cannot resolve host"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn execute_refuses_a_numeric_private_host() {
        // `2130706433` is `127.0.0.1` in decimal. The literal check must
        // refuse it BEFORE the resolve guard runs (and well before any
        // browser is launched), so this test must NOT reach `open_in_brave`.
        let tool = test_tool(vec!["*"]);
        let result = tool
            .execute(json!({"url": "https://2130706433"}))
            .await
            .unwrap();
        assert!(!result.success, "numeric private host must be refused");
        let err = result.error.expect("refusal must carry an error message");
        assert!(
            err.contains("Blocked local/private host"),
            "unexpected error: {err}"
        );
    }
}
