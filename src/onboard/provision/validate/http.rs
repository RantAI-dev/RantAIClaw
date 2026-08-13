use anyhow::{Context, Result};
use std::time::Duration;

#[derive(Debug)]
pub struct ProbeResult {
    pub status: u16,
    pub body: String,
}

/// Scheme, host and port of `url` — path, query and userinfo dropped.
///
/// Probe URLs carry credentials: Telegram puts the bot token in the path and
/// some platforms take the secret as a query parameter. A transport failure
/// turns this context line into a `ProvisionEvent::Message` that the TUI
/// appends to its overlay log and the headless driver prints to stdout, so it
/// must never carry the URL verbatim. Host and port are enough to tell a DNS
/// failure from a TLS one, and neither is secret.
///
/// This is only half the job: `reqwest::Error` renders as `error sending
/// request for url (…)` with the full URL, so callers must also strip it with
/// [`reqwest::Error::without_url`] before attaching context.
fn safe_target(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return "<unparseable url>".to_string();
    };
    let Some(host) = parsed.host_str() else {
        return parsed.scheme().to_string();
    };
    match parsed.port() {
        Some(port) => format!("{}://{host}:{port}", parsed.scheme()),
        None => format!("{}://{host}", parsed.scheme()),
    }
}

pub async fn probe_get(url: &str, headers: &[(&str, &str)]) -> Result<ProbeResult> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()?;
    let mut rb = client.get(url);
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let resp = rb
        .send()
        .await
        .map_err(|e| e.without_url())
        .with_context(|| format!("GET {}", safe_target(url)))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok(ProbeResult { status, body })
}

pub async fn probe_post(url: &str, headers: &[(&str, &str)], body: &str) -> Result<ProbeResult> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()?;
    let mut rb = client.post(url).body(body.to_string());
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let resp = rb
        .send()
        .await
        .map_err(|e| e.without_url())
        .with_context(|| format!("POST {}", safe_target(url)))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    Ok(ProbeResult { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_get_returns_status() {
        let mut mock = mockito::Server::new_async().await;
        let m = mock
            .mock("GET", "/test")
            .with_status(200)
            .create_async()
            .await;
        let url = format!("{}/test", mock.url());
        let r = probe_get(&url, &[]).await.unwrap();
        assert_eq!(r.status, 200);
        m.assert_async().await;
    }

    #[tokio::test]
    async fn probe_post_returns_status() {
        let mut mock = mockito::Server::new_async().await;
        let m = mock
            .mock("POST", "/test")
            .with_status(201)
            .create_async()
            .await;
        let url = format!("{}/test", mock.url());
        let r = probe_post(&url, &[], "").await.unwrap();
        assert_eq!(r.status, 201);
        m.assert_async().await;
    }

    #[tokio::test]
    async fn probe_get_invalid_url() {
        let r = probe_get("http://localhost:99999", &[]).await;
        assert!(r.is_err());
    }

    #[test]
    fn safe_target_drops_path_and_query() {
        assert_eq!(
            safe_target("https://api.example.com/botPLACEHOLDER-TOKEN/getMe"),
            "https://api.example.com"
        );
        assert_eq!(
            safe_target("https://api.example.com/token?appsecret=PLACEHOLDER-SECRET"),
            "https://api.example.com"
        );
        assert_eq!(
            safe_target("http://127.0.0.1:8080/whatever"),
            "http://127.0.0.1:8080"
        );
        assert_eq!(safe_target("not a url"), "<unparseable url>");
    }

    /// A transport failure must not turn a credential-bearing URL into an error
    /// string. The provisioners render this into a `ProvisionEvent::Message`,
    /// which the TUI appends to its overlay log and the headless driver prints.
    #[tokio::test]
    async fn probe_error_context_excludes_the_url() {
        // Port 1 is reserved and closed: connection refused, no DNS lookup.
        let url = "http://127.0.0.1:1/botPLACEHOLDER-TOKEN/getMe?appsecret=PLACEHOLDER-SECRET";
        let err = probe_get(url, &[])
            .await
            .expect_err("a closed port must fail");
        let rendered = format!("{err:#}");

        assert!(
            !rendered.contains("PLACEHOLDER-TOKEN"),
            "path credential leaked into the error: {rendered}"
        );
        assert!(
            !rendered.contains("PLACEHOLDER-SECRET"),
            "query credential leaked into the error: {rendered}"
        );
        assert!(
            !rendered.contains('?'),
            "a query string leaked into the error: {rendered}"
        );
        // Host and port must survive — they are what tells DNS from TLS from refused.
        assert!(
            rendered.contains("127.0.0.1:1"),
            "host lost from the error, leaving nothing to diagnose: {rendered}"
        );
    }

    #[tokio::test]
    async fn probe_post_error_context_excludes_the_url() {
        let url = "http://127.0.0.1:1/v1/token?appsecret=PLACEHOLDER-SECRET";
        let err = probe_post(url, &[], "{}")
            .await
            .expect_err("a closed port must fail");
        let rendered = format!("{err:#}");
        assert!(
            !rendered.contains("PLACEHOLDER-SECRET"),
            "query credential leaked into the error: {rendered}"
        );
        assert!(
            !rendered.contains('?'),
            "a query string leaked into the error: {rendered}"
        );
    }
}
