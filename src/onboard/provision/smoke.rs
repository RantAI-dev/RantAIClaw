//! Headless smoke-test runner for all registered provisioners.
//!
//! Each provisioner is exercised with a deterministic stream of canned
//! responses that walk the "happy path" — the simplest set of inputs that
//! lets the provisioner reach a terminal state (`Done` or `Failed`) without
//! blocking forever or panicking.
//!
//! Run with:
//! ```
//! cargo test --lib onboard::provision::smoke
//! ```
//!
//! Or for a single provisioner:
//! ```
//! cargo test --lib onboard::provision::smoke::telegram
//! ```

use crate::config::Config;
use crate::onboard::provision::registry::{available, provisioner_for};
use crate::onboard::provision::{ProvisionEvent, ProvisionIo, ProvisionResponse};
use crate::profile::Profile;
use std::path::PathBuf;
use tokio::sync::mpsc;

async fn run_provisioner_headless(
    name: &str,
    responses: Vec<ProvisionResponse>,
) -> anyhow::Result<Vec<ProvisionEvent>> {
    let provisioner = provisioner_for(name)
        .ok_or_else(|| anyhow::anyhow!("no provisioner registered: {name}"))?;

    let (event_tx, mut event_rx) = mpsc::channel::<ProvisionEvent>(32);
    let (resp_tx, resp_rx) = mpsc::channel::<ProvisionResponse>(8);

    let mut cfg = Config::default();
    let profile = Profile {
        name: "test".to_string(),
        root: PathBuf::from("/tmp/rantaiclaw-smoke"),
    };
    let io = ProvisionIo {
        events: event_tx,
        responses: resp_rx,
    };

    let handle = tokio::spawn(async move { provisioner.run(&mut cfg, &profile, io).await });

    let mut events = Vec::new();
    let mut resp_idx = 0usize;

    loop {
        tokio::select! {
            ev = event_rx.recv() => {
                match ev {
                    Some(ev) => {
                        events.push(ev.clone());
                        match &ev {
                            ProvisionEvent::Prompt { .. } => {
                                let resp = responses.get(resp_idx).cloned().unwrap_or(ProvisionResponse::Text(String::new()));
                                resp_idx = resp_idx.saturating_add(1);
                                let _ = resp_tx.send(resp).await;
                            }
                            ProvisionEvent::Choose { multi, .. } => {
                                let resp = responses.get(resp_idx).cloned().unwrap_or_else(|| {
                                    if *multi {
                                        ProvisionResponse::Selection(vec![])
                                    } else {
                                        ProvisionResponse::Selection(vec![0])
                                    }
                                });
                                resp_idx = resp_idx.saturating_add(1);
                                let _ = resp_tx.send(resp).await;
                            }
                            ProvisionEvent::Done { .. } | ProvisionEvent::Failed { .. } => {
                                let _ = handle.await;
                                break;
                            }
                            ProvisionEvent::Message { .. } | ProvisionEvent::QrCode { .. } => {}
                        }
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                let _ = handle.abort();
                break;
            }
        }
    }

    Ok(events)
}

fn assert_terminal_event(events: &[ProvisionEvent], name: &str) {
    let has_terminal = events.iter().any(|e| {
        matches!(
            e,
            ProvisionEvent::Done { .. } | ProvisionEvent::Failed { .. }
        )
    });
    assert!(
        has_terminal,
        "provisioner '{name}' never emitted Done or Failed — events: {:#?}",
        events
    );
}

fn default_responses() -> Vec<ProvisionResponse> {
    vec![
        ProvisionResponse::Selection(vec![0]),
        ProvisionResponse::Selection(vec![0]),
        ProvisionResponse::Text(String::new()),
        ProvisionResponse::Text(String::new()),
        ProvisionResponse::Selection(vec![0]),
        ProvisionResponse::Selection(vec![0]),
        ProvisionResponse::Text(String::new()),
        ProvisionResponse::Text(String::new()),
    ]
}

fn assert_no_panic(events: &[ProvisionEvent]) {
    for ev in events {
        if let ProvisionEvent::Failed { error } = ev {
            assert!(
                !error.to_lowercase().contains("panicked"),
                "provisioner panicked: {error}"
            );
        }
    }
}

#[tokio::test]
async fn smoke_all_registered_provisioners() {
    for (name, desc) in available() {
        let responses = super::registry::test_responses_for(name);
        let result = run_provisioner_headless(name, responses).await;
        match result {
            Ok(events) => {
                assert_no_panic(&events);
                assert_terminal_event(&events, name);
            }
            Err(e) => {
                panic!("provisioner '{name}' ({desc}) failed to run: {e}");
            }
        }
    }
}

// ── Individual provisioner smoke tests ────────────────────────────────────────

mod persona {
    use super::*;

    #[tokio::test]
    async fn persona_completes() {
        let events = run_provisioner_headless(
            "persona",
            vec![
                ProvisionResponse::Selection(vec![0]),
                ProvisionResponse::Text(String::new()),
                ProvisionResponse::Selection(vec![0]),
                ProvisionResponse::Text(String::new()),
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "persona");
    }
}

mod provider {
    use super::*;

    #[tokio::test]
    async fn provider_completes() {
        let events = run_provisioner_headless(
            "provider",
            vec![
                ProvisionResponse::Selection(vec![0]),
                ProvisionResponse::Selection(vec![0]),
                ProvisionResponse::Text(String::new()),
                ProvisionResponse::Text(String::new()),
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "provider");
    }
}

mod approvals {
    use super::*;

    #[tokio::test]
    async fn approvals_completes() {
        let events = run_provisioner_headless(
            "approvals",
            vec![
                ProvisionResponse::Selection(vec![0]), // L1 preset
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "approvals");
    }
}

mod mcp {
    use super::*;

    #[tokio::test]
    async fn mcp_completes() {
        let events = run_provisioner_headless(
            "mcp",
            vec![
                ProvisionResponse::Selection(vec![]),
                ProvisionResponse::Selection(vec![0]),
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "mcp");
    }
}

mod skills {
    use super::*;

    #[tokio::test]
    async fn skills_completes() {
        let events = run_provisioner_headless(
            "skills",
            vec![
                ProvisionResponse::Selection(vec![0]), // install starter pack
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "skills");
    }
}

mod telegram {
    use super::*;

    #[tokio::test]
    async fn telegram_completes() {
        let events = run_provisioner_headless(
            "telegram",
            vec![
                ProvisionResponse::Text(String::new()), // bot token
                ProvisionResponse::Selection(vec![0]),  // enable/disable
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "telegram");
    }
}

mod discord {
    use super::*;

    #[tokio::test]
    async fn discord_completes() {
        let events = run_provisioner_headless(
            "discord",
            vec![
                ProvisionResponse::Text(String::new()), // bot token
                ProvisionResponse::Selection(vec![0]),  // enable/disable
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "discord");
    }
}

mod slack {
    use super::*;

    #[tokio::test]
    async fn slack_completes() {
        let events = run_provisioner_headless(
            "slack",
            vec![
                ProvisionResponse::Text(String::new()), // bot token
                ProvisionResponse::Text(String::new()), // signing secret
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "slack");
    }
}

mod signal {
    use super::*;

    #[tokio::test]
    async fn signal_completes() {
        let events =
            run_provisioner_headless("signal", vec![ProvisionResponse::Text(String::new())])
                .await
                .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "signal");
    }
}

mod matrix {
    use super::*;

    #[tokio::test]
    async fn matrix_completes() {
        let events = run_provisioner_headless(
            "matrix",
            vec![
                ProvisionResponse::Text(String::new()), // homeserver
                ProvisionResponse::Text(String::new()), // user_id
                ProvisionResponse::Text(String::new()), // password/token
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "matrix");
    }
}

mod mattermost {
    use super::*;

    #[tokio::test]
    async fn mattermost_completes() {
        let events = run_provisioner_headless(
            "mattermost",
            vec![
                ProvisionResponse::Text(String::new()), // server URL
                ProvisionResponse::Text(String::new()), // token
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "mattermost");
    }
}

mod memory {
    use super::*;

    #[tokio::test]
    async fn memory_completes() {
        let events = run_provisioner_headless(
            "memory",
            vec![
                ProvisionResponse::Selection(vec![0]),  // backend type
                ProvisionResponse::Text(String::new()), // path
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "memory");
    }
}

mod runtime {
    use super::*;

    #[tokio::test]
    async fn runtime_completes() {
        let events = run_provisioner_headless(
            "runtime",
            vec![
                ProvisionResponse::Selection(vec![0]), // runtime type
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "runtime");
    }
}

mod proxy {
    use super::*;

    #[tokio::test]
    async fn proxy_completes() {
        let events = run_provisioner_headless(
            "proxy",
            vec![
                ProvisionResponse::Selection(vec![0]),  // enable scope
                ProvisionResponse::Text(String::new()), // http proxy URL
                ProvisionResponse::Text(String::new()), // https proxy URL
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "proxy");
    }
}

mod tunnel {
    use super::*;

    #[tokio::test]
    async fn tunnel_completes() {
        let events = run_provisioner_headless(
            "tunnel",
            vec![
                ProvisionResponse::Selection(vec![0]), // tunnel type (None)
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "tunnel");
    }
}

mod gateway {
    use super::*;

    #[tokio::test]
    async fn gateway_completes() {
        let events = run_provisioner_headless(
            "gateway",
            vec![
                ProvisionResponse::Selection(vec![0]), // enable/disable
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "gateway");
    }
}

mod browser {
    use super::*;

    #[tokio::test]
    async fn browser_completes() {
        let events = run_provisioner_headless(
            "browser",
            vec![
                ProvisionResponse::Selection(vec![0]), // enable/disable
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "browser");
    }
}

mod web_search {
    use super::*;

    #[tokio::test]
    async fn web_search_completes() {
        let events = run_provisioner_headless(
            "web-search",
            vec![
                ProvisionResponse::Selection(vec![0]),  // provider
                ProvisionResponse::Text(String::new()), // max results
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "web-search");
    }
}

mod composio {
    use super::*;

    #[tokio::test]
    async fn composio_completes() {
        let events = run_provisioner_headless(
            "composio",
            vec![
                ProvisionResponse::Text(String::new()), // api key (empty = will emit Failed, which is ok)
            ],
        )
        .await
        .unwrap();
        // Empty API key emits Failed, which is acceptable for a smoke test.
        assert!(events.iter().any(|e| matches!(
            e,
            ProvisionEvent::Done { .. } | ProvisionEvent::Failed { .. }
        )));
    }
}

mod agents {
    use super::*;

    #[tokio::test]
    async fn agents_completes() {
        let events = run_provisioner_headless(
            "agents",
            vec![
                ProvisionResponse::Selection(vec![]), // no built-in agents selected
                ProvisionResponse::Selection(vec![0]), // add custom? (No)
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "agents");
    }
}

mod model_routes {
    use super::*;

    #[tokio::test]
    async fn model_routes_completes() {
        let events = run_provisioner_headless(
            "model-routes",
            vec![
                ProvisionResponse::Selection(vec![1]), // Done (don't add a route)
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "model-routes");
    }
}

mod embedding_routes {
    use super::*;

    #[tokio::test]
    async fn embedding_routes_completes() {
        let events = run_provisioner_headless(
            "embedding-routes",
            vec![
                ProvisionResponse::Selection(vec![1]), // Done (don't add a route)
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "embedding-routes");
    }
}

mod secrets {
    use super::*;

    #[tokio::test]
    async fn secrets_completes() {
        let events = run_provisioner_headless(
            "secrets",
            vec![
                ProvisionResponse::Selection(vec![0]), // enable encryption
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "secrets");
    }
}

mod multimodal {
    use super::*;

    #[tokio::test]
    async fn multimodal_completes() {
        let events = run_provisioner_headless(
            "multimodal",
            vec![
                ProvisionResponse::Text(String::new()), // max_images default
                ProvisionResponse::Text(String::new()), // max_image_size_mb default
                ProvisionResponse::Selection(vec![0]),  // allow remote fetch
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "multimodal");
    }
}

mod hardware {
    use super::*;

    #[tokio::test]
    async fn hardware_completes() {
        let events = run_provisioner_headless(
            "hardware",
            vec![
                ProvisionResponse::Selection(vec![0]), // disabled
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "hardware");
    }
}
