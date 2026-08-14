//! Headless smoke-test runner for all registered provisioners.
//!
//! Each provisioner is exercised with a deterministic stream of canned
//! responses that walk the "happy path" — the simplest set of inputs that
//! lets the provisioner reach a terminal state (`Done` or `Failed`) without
//! blocking forever or panicking.
//!
//! Run with:
//! ```text
//! cargo test --lib onboard::provision::smoke
//! ```
//!
//! Or for a single provisioner:
//! ```text
//! cargo test --lib onboard::provision::smoke::telegram
//! ```

// Each per-provisioner test module does `use super::*;` so it can reach
// the shared helpers and `ProvisionResponse`/`ProvisionEvent` types. Some
// mods don't exercise every helper — Rust warns on the unused subset
// per-mod. Allow at the file level so we don't have to spray
// `#[allow(unused_imports)]` on 17 test modules.
#![allow(unused_imports)]

use crate::config::Config;
use crate::onboard::provision::registry::{available, provisioner_for};
use crate::onboard::provision::{ProvisionEvent, ProvisionIo, ProvisionOutcome, ProvisionResponse};
use crate::profile::Profile;
use std::path::PathBuf;
use tokio::sync::mpsc;

async fn run_provisioner_headless(
    name: &str,
    responses: Vec<ProvisionResponse>,
) -> anyhow::Result<Vec<ProvisionEvent>> {
    // Provisioners write through `profile::paths`, which resolves the home
    // directory on every call. Any test that overrides `HOME` to isolate
    // itself therefore moves the ground under these runs mid-flight — taking
    // the crate-shared lock here serialises against those instead of racing
    // them. Held for the whole run because the writes happen inside it.
    //
    // Gated because `test_env` is itself `#[cfg(test)]` while this module is
    // not; every caller is a test, so the lock is always taken in practice.
    #[cfg(test)]
    let _env = crate::test_env::ENV_LOCK.lock().await;

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
            () = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                let () = handle.abort();
                break;
            }
        }
    }

    Ok(events)
}

/// Drive a registered provisioner and hand back what it decided *and* what it
/// wrote, so a caller can assert on both.
///
/// `wrote` reads the channel's own slot out of the config the provisioner
/// mutated — the old runner dropped that config without looking at it, which
/// is why a provisioner that wrote nothing still passed.
async fn run_and_capture(
    name: &str,
    responses: Vec<ProvisionResponse>,
    wrote: fn(&Config) -> bool,
) -> (anyhow::Result<ProvisionOutcome>, Vec<ProvisionEvent>, bool) {
    #[cfg(test)]
    let _env = crate::test_env::ENV_LOCK.lock().await;
    let _offline = OfflineProbes::engage();

    let Some(provisioner) = provisioner_for(name) else {
        return (
            Err(anyhow::anyhow!("no provisioner registered: {name}")),
            Vec::new(),
            false,
        );
    };

    let (event_tx, mut event_rx) = mpsc::channel::<ProvisionEvent>(64);
    let (resp_tx, resp_rx) = mpsc::channel::<ProvisionResponse>(64);
    let profile = Profile {
        name: "provisioning-smoke".to_string(),
        root: PathBuf::from("/tmp/rantaiclaw-smoke"),
    };
    let io = ProvisionIo {
        events: event_tx,
        responses: resp_rx,
    };

    let handle = tokio::spawn(async move {
        let mut cfg = Config::default();
        let outcome = provisioner.run(&mut cfg, &profile, io).await;
        (outcome, cfg)
    });

    // Answer prompts from the script, in order. Anything past the end gets an
    // empty answer, which every required field rejects — so a script that is
    // too short shows up as an abort rather than as a hang.
    let mut events = Vec::new();
    let mut idx = 0usize;
    while let Some(ev) = event_rx.recv().await {
        events.push(ev.clone());
        let reply = match &ev {
            ProvisionEvent::Prompt { .. } => Some(
                responses
                    .get(idx)
                    .cloned()
                    .unwrap_or(ProvisionResponse::Text(String::new())),
            ),
            ProvisionEvent::Choose { multi, .. } => Some(responses.get(idx).cloned().unwrap_or(
                ProvisionResponse::Selection(if *multi { vec![] } else { vec![0] }),
            )),
            _ => None,
        };
        if let Some(r) = reply {
            idx = idx.saturating_add(1);
            let _ = resp_tx.send(r).await;
        }
    }

    match handle.await {
        Ok((outcome, cfg)) => {
            let written = wrote(&cfg);
            (outcome, events, written)
        }
        Err(e) => (
            Err(anyhow::anyhow!("provisioner task died: {e}")),
            events,
            false,
        ),
    }
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

// ── Outcome-and-config assertions ─────────────────────────────────────────────
//
// `assert_terminal_event` accepts `Failed` as a pass, and the config it ran
// against was dropped without being looked at. Combined with a first response
// of `Text("")` — which every channel's first prompt rejects as a missing
// required field — no line of credential collection, probing, allowlist
// parsing or config writing was ever executed. The pair below is what makes an
// abort distinguishable from a success.

/// A run that configured the channel: `Configured`, and the config section is
/// actually populated.
fn assert_configured(
    outcome: &anyhow::Result<ProvisionOutcome>,
    written: bool,
    name: &str,
    events: &[ProvisionEvent],
) {
    match outcome {
        Ok(ProvisionOutcome::Configured) => {}
        other => panic!("'{name}' should have configured; got {other:?}\nevents: {events:#?}"),
    }
    assert!(
        written,
        "'{name}' reported Configured but wrote no config section"
    );
}

/// A run that stopped early: an abort, and the config section is untouched.
fn assert_aborted(
    outcome: &anyhow::Result<ProvisionOutcome>,
    written: bool,
    name: &str,
    events: &[ProvisionEvent],
) {
    match outcome {
        Ok(ProvisionOutcome::Aborted(_)) => {}
        other => panic!("'{name}' should have aborted; got {other:?}\nevents: {events:#?}"),
    }
    assert!(
        !written,
        "'{name}' aborted but still wrote a config section"
    );
}

/// Forces every credential probe to fail at the transport layer, whatever the
/// runner's connectivity.
///
/// Without this the smoke tests are non-deterministic in the worst way: a
/// runner *with* egress gets a real 401 from the platform — a `Rejected`
/// verdict, whose safe default is to discard — while a runner *without* egress
/// gets a transport error — `Inconclusive`, whose safe default is to persist.
/// The same test would pass on one and fail on the other.
///
/// Port 1 is reserved and closed, so the proxy connect is refused instantly:
/// no DNS, no eight-second timeout, and the probe reaches the provisioner as
/// the inconclusive result a happy-path smoke run wants.
struct OfflineProbes {
    prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl OfflineProbes {
    fn engage() -> Self {
        let mut prev = Vec::new();
        for key in ["HTTPS_PROXY", "HTTP_PROXY"] {
            prev.push((key, std::env::var_os(key)));
            std::env::set_var(key, "http://127.0.0.1:1");
        }
        Self { prev }
    }
}

impl Drop for OfflineProbes {
    fn drop(&mut self) {
        for (key, value) in self.prev.drain(..) {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
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

/// A breadth sweep: every registered provisioner reaches a terminal state
/// without panicking.
///
/// This is deliberately the weaker of the two layers. It cannot assert what a
/// provisioner *wrote*, because it discards the config — so the fifteen
/// channels each carry their own happy-path and abort pair below, where the
/// config is captured and asserted on. Keeping the sweep as well is what
/// covers the twenty-odd non-channel provisioners, and what catches a newly
/// registered provisioner that hangs.
#[tokio::test]
async fn smoke_all_registered_provisioners() {
    for (name, desc) in available() {
        // whatsapp-web cannot be smoked headless: its provision flow IS a live
        // WhatsApp Web pairing (real network handshake, then it waits for a QR
        // scan that never comes), so it hits the 10s abort with no terminal
        // event. The per-provisioner smoke modules below deliberately omit it
        // for the same reason. On loaded runners the connect happened to fail
        // fast (Failed = terminal), which is why this only flakes on quiet runs.
        if name == "whatsapp-web" {
            continue;
        }
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
                ProvisionResponse::Selection(vec![0]),  // tier
                ProvisionResponse::Selection(vec![0]),  // specific provider
                ProvisionResponse::Text(String::new()), // api key (empty)
                ProvisionResponse::Selection(vec![0]),  // default model (Choose)
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "provider");
    }
}

mod login {
    use super::*;

    #[tokio::test]
    async fn login_completes() {
        let events = run_provisioner_headless(
            "login",
            vec![
                ProvisionResponse::Selection(vec![0]),             // enable
                ProvisionResponse::Text("rantaiclaw_user".into()), // username
                ProvisionResponse::Text("smoke-pass".into()),      // password
                ProvisionResponse::Text("smoke-pass".into()),      // confirm (matches)
                ProvisionResponse::Selection(vec![0]),             // idle auto-lock: never
            ],
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "login");
    }

    #[tokio::test]
    async fn login_skip_leaves_disabled() {
        let events = run_provisioner_headless(
            "login",
            vec![ProvisionResponse::Selection(vec![1])], // skip / disable
        )
        .await
        .unwrap();
        assert_no_panic(&events);
        assert_terminal_event(&events, "login");
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
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.telegram.is_some()
    }

    #[tokio::test]
    async fn telegram_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "telegram",
            vec![
                ProvisionResponse::Text("00000000:placeholder-bot-token".into()),
                ProvisionResponse::Selection(vec![0]), // probe inconclusive → save anyway
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed users
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "telegram", &events);
    }

    #[tokio::test]
    async fn telegram_aborts_without_a_token() {
        let (outcome, events, written) = run_and_capture(
            "telegram",
            vec![ProvisionResponse::Text(String::new())],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "telegram", &events);
    }
}

mod discord {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.discord.is_some()
    }

    #[tokio::test]
    async fn discord_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "discord",
            vec![
                ProvisionResponse::Text("placeholder-discord-bot-token".into()), // bot token
                ProvisionResponse::Selection(vec![0]), // probe inconclusive -> save anyway
                ProvisionResponse::Text(String::new()), // guild id (optional)
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed users
                ProvisionResponse::Selection(vec![0]), // bot mode
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "discord", &events);
    }

    #[tokio::test]
    async fn discord_aborts_on_a_missing_required_field() {
        let (outcome, events, written) = run_and_capture(
            "discord",
            vec![ProvisionResponse::Text(String::new())],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "discord", &events);
    }
}

mod slack {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.slack.is_some()
    }

    #[tokio::test]
    async fn slack_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "slack",
            vec![
                // No `xoxb-` prefix on purpose: secret scanners match on the
                // prefix, and this repo's fixtures have been rejected at push
                // time for looking real. The provisioner does not check shape.
                ProvisionResponse::Text("placeholder-slack-bot-token".into()), // bot token
                ProvisionResponse::Text(String::new()), // app token (optional)
                ProvisionResponse::Selection(vec![0]),  // probe inconclusive -> save anyway
                ProvisionResponse::Text(String::new()), // default channel (optional)
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed users
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "slack", &events);
    }

    #[tokio::test]
    async fn slack_aborts_on_a_missing_required_field() {
        let (outcome, events, written) =
            run_and_capture("slack", vec![ProvisionResponse::Text(String::new())], wrote).await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "slack", &events);
    }
}

mod signal {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.signal.is_some()
    }

    #[tokio::test]
    async fn signal_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "signal",
            vec![
                ProvisionResponse::Text("http://127.0.0.1:1".into()), // signal-cli daemon url
                ProvisionResponse::Text("+15550001111".into()),       // account
                ProvisionResponse::Selection(vec![0]), // probe inconclusive -> save anyway
                ProvisionResponse::Text("+15550002222".into()), // allowed senders
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "signal", &events);
    }

    #[tokio::test]
    async fn signal_aborts_on_a_missing_required_field() {
        let (outcome, events, written) = run_and_capture(
            "signal",
            vec![ProvisionResponse::Text(String::new())],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "signal", &events);
    }
}

mod matrix {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.matrix.is_some()
    }

    #[tokio::test]
    async fn matrix_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "matrix",
            vec![
                ProvisionResponse::Text("http://127.0.0.1:1".into()), // homeserver
                ProvisionResponse::Text("placeholder-matrix-access-token".into()), // access token
                ProvisionResponse::Selection(vec![0]), // probe inconclusive -> save anyway
                ProvisionResponse::Text("@rantaiclaw_user:example.com".into()), // user id
                ProvisionResponse::Text(String::new()), // device id (optional)
                ProvisionResponse::Text("!placeholder:example.com".into()), // room id
                ProvisionResponse::Text("@rantaiclaw_user:example.com".into()), // allowed users
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "matrix", &events);
    }

    #[tokio::test]
    async fn matrix_aborts_on_a_missing_required_field() {
        let (outcome, events, written) = run_and_capture(
            "matrix",
            vec![ProvisionResponse::Text(String::new())],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "matrix", &events);
    }
}

mod mattermost {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.mattermost.is_some()
    }

    #[tokio::test]
    async fn mattermost_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "mattermost",
            vec![
                ProvisionResponse::Text("http://127.0.0.1:1".into()), // server url
                ProvisionResponse::Text("placeholder-mattermost-bot-token".into()), // bot token
                ProvisionResponse::Selection(vec![0]), // probe inconclusive -> save anyway
                ProvisionResponse::Text(String::new()), // default channel (optional)
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed users
                ProvisionResponse::Selection(vec![0]), // thread replies
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "mattermost", &events);
    }

    #[tokio::test]
    async fn mattermost_aborts_on_a_missing_required_field() {
        let (outcome, events, written) = run_and_capture(
            "mattermost",
            vec![ProvisionResponse::Text(String::new())],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "mattermost", &events);
    }
}

mod dingtalk {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.dingtalk.is_some()
    }

    #[tokio::test]
    async fn dingtalk_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "dingtalk",
            vec![
                ProvisionResponse::Text("placeholder-client-id".into()), // client id
                ProvisionResponse::Text("placeholder-client-secret".into()), // client secret
                ProvisionResponse::Selection(vec![0]), // probe inconclusive -> save anyway
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed users
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "dingtalk", &events);
    }

    #[tokio::test]
    async fn dingtalk_aborts_on_a_missing_required_field() {
        let (outcome, events, written) = run_and_capture(
            "dingtalk",
            vec![ProvisionResponse::Text(String::new())],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "dingtalk", &events);
    }
}

mod nextcloud_talk {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.nextcloud_talk.is_some()
    }

    #[tokio::test]
    async fn nextcloud_talk_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "nextcloud-talk",
            vec![
                ProvisionResponse::Text("http://127.0.0.1:1".into()), // base url
                ProvisionResponse::Text("placeholder-app-token".into()), // app token
                ProvisionResponse::Selection(vec![0]), // probe inconclusive -> save anyway
                ProvisionResponse::Text(String::new()), // webhook secret (optional)
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed users
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "nextcloud-talk", &events);
    }

    #[tokio::test]
    async fn nextcloud_talk_aborts_on_a_missing_required_field() {
        let (outcome, events, written) = run_and_capture(
            "nextcloud-talk",
            vec![ProvisionResponse::Text(String::new())],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "nextcloud-talk", &events);
    }
}

mod qq {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.qq.is_some()
    }

    #[tokio::test]
    async fn qq_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "qq",
            vec![
                ProvisionResponse::Text("placeholder-app-id".into()), // app id
                ProvisionResponse::Text("placeholder-app-secret".into()), // app secret
                ProvisionResponse::Selection(vec![0]), // probe inconclusive -> save anyway
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed users
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "qq", &events);
    }

    #[tokio::test]
    async fn qq_aborts_on_a_missing_required_field() {
        let (outcome, events, written) =
            run_and_capture("qq", vec![ProvisionResponse::Text(String::new())], wrote).await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "qq", &events);
    }
}

mod whatsapp_cloud {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.whatsapp.is_some()
    }

    #[tokio::test]
    async fn whatsapp_cloud_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "whatsapp-cloud",
            vec![
                ProvisionResponse::Text("placeholder-access-token".into()), // access token
                ProvisionResponse::Text("000000000000000".into()),          // phone number id
                ProvisionResponse::Selection(vec![0]), // probe inconclusive -> save anyway
                ProvisionResponse::Text("placeholder-verify-token".into()), // verify token
                ProvisionResponse::Text(String::new()), // app secret (optional)
                ProvisionResponse::Text("+15550002222".into()), // allowed numbers
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "whatsapp-cloud", &events);
    }

    #[tokio::test]
    async fn whatsapp_cloud_aborts_on_a_missing_required_field() {
        let (outcome, events, written) = run_and_capture(
            "whatsapp-cloud",
            vec![ProvisionResponse::Text(String::new())],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "whatsapp-cloud", &events);
    }
}

mod linq {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.linq.is_some()
    }

    #[tokio::test]
    async fn linq_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "linq",
            vec![
                ProvisionResponse::Text("placeholder-partner-api-token".into()), // api token
                ProvisionResponse::Selection(vec![0]), // probe inconclusive -> save anyway
                ProvisionResponse::Text("+15550001111".into()), // from phone
                ProvisionResponse::Text(String::new()), // signing secret (optional)
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed senders
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "linq", &events);
    }

    #[tokio::test]
    async fn linq_aborts_on_a_missing_required_field() {
        let (outcome, events, written) =
            run_and_capture("linq", vec![ProvisionResponse::Text(String::new())], wrote).await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "linq", &events);
    }
}

mod lark {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.lark.is_some()
    }

    #[tokio::test]
    async fn lark_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "lark",
            vec![
                ProvisionResponse::Text("placeholder-app-id".into()), // app id
                ProvisionResponse::Text("placeholder-app-secret".into()), // app secret
                ProvisionResponse::Selection(vec![1]),                // region: Lark International
                ProvisionResponse::Selection(vec![0]), // probe inconclusive -> save anyway
                ProvisionResponse::Text(String::new()), // verification token (optional)
                ProvisionResponse::Selection(vec![0]), // receive mode: websocket
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed users
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "lark", &events);
    }

    #[tokio::test]
    async fn lark_aborts_on_a_missing_required_field() {
        let (outcome, events, written) =
            run_and_capture("lark", vec![ProvisionResponse::Text(String::new())], wrote).await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "lark", &events);
    }
}

mod irc {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.irc.is_some()
    }

    #[tokio::test]
    async fn irc_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "irc",
            vec![
                ProvisionResponse::Text("irc.example.com".into()), // server
                ProvisionResponse::Text(String::new()),            // port -> default
                ProvisionResponse::Text("rantaiclaw_bot".into()),  // nickname
                ProvisionResponse::Text(String::new()),            // username -> nickname
                ProvisionResponse::Selection(vec![0]),             // TLS yes
                ProvisionResponse::Text(String::new()),            // server password (skip)
                ProvisionResponse::Text(String::new()),            // nickserv password (skip)
                ProvisionResponse::Text(String::new()),            // sasl password (skip)
                ProvisionResponse::Text("#rantaiclaw".into()),     // channels
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed nicknames
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "irc", &events);
    }

    #[tokio::test]
    async fn irc_aborts_on_a_missing_required_field() {
        let (outcome, events, written) =
            run_and_capture("irc", vec![ProvisionResponse::Text(String::new())], wrote).await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "irc", &events);
    }
}

mod email {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.email.is_some()
    }

    #[tokio::test]
    async fn email_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "email",
            vec![
                ProvisionResponse::Text("imap.example.com".into()), // imap host
                ProvisionResponse::Text(String::new()),             // imap port -> default
                ProvisionResponse::Text(String::new()),             // imap folder -> INBOX
                ProvisionResponse::Text("smtp.example.com".into()), // smtp host
                ProvisionResponse::Text(String::new()),             // smtp port -> default
                ProvisionResponse::Text("bot@example.com".into()),  // from address
                ProvisionResponse::Text(String::new()),             // username -> from address
                ProvisionResponse::Text("placeholder-app-password".into()), // password
                ProvisionResponse::Text("bot@example.com".into()),  // allowed senders
                ProvisionResponse::Text(String::new()),             // idle timeout -> default
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "email", &events);
    }

    #[tokio::test]
    async fn email_aborts_on_a_missing_required_field() {
        let (outcome, events, written) =
            run_and_capture("email", vec![ProvisionResponse::Text(String::new())], wrote).await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "email", &events);
    }
}

// iMessage has no credential to probe; its abort path is declining the prerequisites.
mod imessage {
    use super::{
        assert_aborted, assert_configured, assert_no_panic, run_and_capture, Config,
        ProvisionEvent, ProvisionResponse,
    };

    fn wrote(c: &Config) -> bool {
        c.channels_config.imessage.is_some()
    }

    /// iMessage is the one channel with no happy path off macOS: the
    /// provisioner refuses on any other platform before it prompts at all.
    /// Gated rather than deleted so the case is still covered where it can be.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn imessage_happy_path_writes_its_config() {
        let (outcome, events, written) = run_and_capture(
            "imessage",
            vec![
                ProvisionResponse::Selection(vec![0]), // prerequisites confirmed
                ProvisionResponse::Text("rantaiclaw_user".into()), // allowed contacts
            ],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_configured(&outcome, written, "imessage", &events);
    }

    /// Declining the prerequisites must write nothing.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn imessage_aborts_when_prerequisites_are_declined() {
        let (outcome, events, written) = run_and_capture(
            "imessage",
            vec![ProvisionResponse::Selection(vec![1])],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "imessage", &events);
    }

    /// Everywhere else the refusal itself is the contract: abort, and no
    /// config section — not a half-written one the runtime would later fail on.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn imessage_aborts_off_macos_without_writing() {
        let (outcome, events, written) = run_and_capture(
            "imessage",
            vec![ProvisionResponse::Selection(vec![0])],
            wrote,
        )
        .await;
        assert_no_panic(&events);
        assert_aborted(&outcome, written, "imessage", &events);
        assert!(
            events.iter().any(|e| matches!(
                e,
                ProvisionEvent::Failed { error } if error.contains("macOS")
            )),
            "the refusal must say why: {events:#?}"
        );
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
