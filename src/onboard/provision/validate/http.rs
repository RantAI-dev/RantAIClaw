use anyhow::{Context, Result};
use std::time::Duration;

pub struct ProbeResult {
    pub status: u16,
    pub body: String,
}

pub async fn probe_get(url: &str, headers: &[(&str, &str)]) -> Result<ProbeResult> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()?;
    let mut rb = client.get(url);
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let resp = rb.send().await.with_context(|| format!("GET {url}"))?;
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
    let resp = rb.send().await.with_context(|| format!("POST {url}"))?;
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
}
