//! WhatsApp Web's HTTP transport, backed by `reqwest`.
//!
//! Replaces `wa-rs-ureq-http`. The size win — dropping `ureq` and `ureq-proto`
//! — is incidental; the reason is that the ureq client was built with no proxy
//! configuration at all, so **WhatsApp Web traffic bypassed `[proxy]`** while
//! every other channel honoured it. An operator who routes the agent through a
//! proxy had one channel quietly going direct.
//!
//! Two clients, one policy. `execute` is async and uses the shared
//! `build_runtime_proxy_client`. `execute_streaming` must hand back a
//! synchronous reader — `wa-rs` streams the encrypted media body straight into
//! a writer from inside `spawn_blocking`, so buffering it to satisfy an async
//! client would trade the proxy fix for a memory regression on large media.
//! Both take their proxies from `ProxyConfig::proxies_for`, so the two cannot
//! disagree about where traffic goes.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use wa_rs_core::net::{HttpClient, HttpRequest, HttpResponse, StreamingHttpResponse};

/// The `[proxy]` service key this channel's traffic is matched against.
const SERVICE_KEY: &str = "channel.whatsapp_web";

pub struct ReqwestHttpClient;

impl ReqwestHttpClient {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// A blocking client carrying the same proxies as the async one.
    fn blocking_client() -> Result<reqwest::blocking::Client> {
        crate::config::apply_runtime_proxy_to_blocking_builder(
            reqwest::blocking::Client::builder(),
            SERVICE_KEY,
        )
        .build()
        .map_err(|e| anyhow!("could not build the WhatsApp Web HTTP client: {e}"))
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        let client = crate::config::build_runtime_proxy_client(SERVICE_KEY);

        // Same two methods the ureq transport accepted, and the same explicit
        // refusal for anything else rather than a silent GET.
        let mut builder = match request.method.as_str() {
            "GET" => client.get(&request.url),
            "POST" => client.post(&request.url),
            method => return Err(anyhow!("Unsupported HTTP method: {method}")),
        };
        for (key, value) in &request.headers {
            builder = builder.header(key, value);
        }
        if request.method == "POST" {
            builder = builder.body(request.body.unwrap_or_default());
        }

        let response = builder.send().await?;
        let status_code = response.status().as_u16();
        let body = response.bytes().await?.to_vec();

        Ok(HttpResponse { status_code, body })
    }

    fn execute_streaming(&self, request: HttpRequest) -> Result<StreamingHttpResponse> {
        // No `spawn_blocking` here — `wa-rs`'s download path already calls this
        // from inside one, and the whole fetch-plus-decrypt runs on that thread.
        if request.method != "GET" {
            return Err(anyhow!(
                "Streaming only supports GET, got: {}",
                request.method
            ));
        }

        let client = Self::blocking_client()?;
        let mut builder = client.get(&request.url);
        for (key, value) in &request.headers {
            builder = builder.header(key, value);
        }

        let response = builder.send()?;
        let status_code = response.status().as_u16();

        Ok(StreamingHttpResponse {
            status_code,
            body: Box::new(response),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{ProxyConfig, ProxyScope};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A stand-in HTTP proxy: it answers `GET http://host/path` (the absolute
    /// form a client uses when it is talking to a proxy) and counts the hits.
    /// Reaching it at all is the assertion — a client that ignores `[proxy]`
    /// dials the origin directly and this counter stays at zero.
    async fn spawn_recording_proxy(hits: &'static AtomicUsize) -> String {
        let app = axum::Router::new().fallback(move || async move {
            hits.fetch_add(1, Ordering::SeqCst);
            "through the proxy"
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn proxy_everything_through(url: &str) -> ProxyConfig {
        ProxyConfig {
            enabled: true,
            http_proxy: Some(url.to_string()),
            scope: ProxyScope::Rantaiclaw,
            ..Default::default()
        }
    }

    /// The reason this transport exists. `wa-rs-ureq-http` built its client with
    /// no proxy configuration at all, so WhatsApp Web went direct while every
    /// other channel honoured `[proxy]`.
    ///
    /// The streaming path is the one that matters: it needs a *blocking* client,
    /// which is a second client and therefore the place the two could drift.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_streaming_download_goes_through_the_configured_proxy() {
        static HITS: AtomicUsize = AtomicUsize::new(0);
        let _guard = crate::test_env::ENV_LOCK.lock().await;

        let proxy_url = spawn_recording_proxy(&HITS).await;
        crate::config::set_runtime_proxy_config(proxy_everything_through(&proxy_url));

        // Port 1 on loopback: nothing listens there, so a direct dial cannot
        // succeed. Only a request that went through the proxy can.
        let request = HttpRequest::get("http://127.0.0.1:1/media.enc");
        let result = tokio::task::spawn_blocking(move || {
            ReqwestHttpClient::new().execute_streaming(request)
        })
        .await
        .expect("join");

        assert!(
            result.is_ok(),
            "the streaming fetch should have been proxied, got: {:?}",
            result.err()
        );
        assert_eq!(
            HITS.load(Ordering::SeqCst),
            1,
            "the media download bypassed [proxy] — this is exactly what the ureq \
             transport did"
        );

        crate::config::set_runtime_proxy_config(ProxyConfig::default());
    }

    /// Same for the non-streaming half, so the async and blocking clients cannot
    /// disagree about where traffic goes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_async_request_goes_through_the_configured_proxy() {
        static HITS: AtomicUsize = AtomicUsize::new(0);
        let _guard = crate::test_env::ENV_LOCK.lock().await;

        let proxy_url = spawn_recording_proxy(&HITS).await;
        crate::config::set_runtime_proxy_config(proxy_everything_through(&proxy_url));

        let response = ReqwestHttpClient::new()
            .execute(HttpRequest::get("http://127.0.0.1:1/meta"))
            .await;

        assert!(response.is_ok(), "got: {:?}", response.err());
        assert_eq!(
            HITS.load(Ordering::SeqCst),
            1,
            "the async request bypassed [proxy]"
        );

        crate::config::set_runtime_proxy_config(ProxyConfig::default());
    }

    #[test]
    fn streaming_refuses_a_non_get_rather_than_silently_downgrading() {
        let mut request = HttpRequest::get("http://example.test/x");
        request.method = "POST".to_string();
        let Err(err) = ReqwestHttpClient::new().execute_streaming(request) else {
            panic!("POST must be refused, not silently downgraded to GET");
        };
        assert!(
            err.to_string().contains("Streaming only supports GET"),
            "{err}"
        );
    }
}
