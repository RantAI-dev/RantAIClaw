//! Integration tests for `src/doctor/` — the trait, runner, renderers,
//! and a mockito-backed provider ping.
//!
//! Snapshot tests are pinned via [`insta`]. Update them with
//! `cargo insta review` when output format changes intentionally.

// Mutex held across .await is intentional — serializes mockito's global state
// across tests. The field-reassign pattern in `ctx_with_provider` is clearer
// than a struct literal given the optional api_key.
#![allow(clippy::await_holding_lock, clippy::field_reassign_with_default)]

use std::sync::Mutex;

use rantaiclaw::config::Config;
use rantaiclaw::doctor::checks::policy::{diagnose_allowlist, AllowlistDiagnosis};
use rantaiclaw::doctor::checks::provider::ProviderPingCheck;
use rantaiclaw::doctor::report::{render, render_brief, render_json, render_text, DoctorFormat};
use rantaiclaw::doctor::{CheckResult, DoctorCheck, DoctorContext, Severity};
use rantaiclaw::profile::Profile;
use tempfile::TempDir;

/// `mockito::Server::new_async` mutates global state; serialize the few
/// tests that drive it.
static MOCKITO_LOCK: Mutex<()> = Mutex::new(());

fn fixture_results() -> Vec<CheckResult> {
    vec![
        CheckResult::ok("config.schema", "config schema is valid"),
        CheckResult::warn("policy.allowlist", "allowlist is empty")
            .with_hint("run: rantaiclaw setup approvals"),
        CheckResult::fail("provider.ping", "401 unauthorized")
            .with_category("live")
            .with_hint("re-enter API key with: rantaiclaw setup provider"),
        CheckResult::info("daemon.registration", "init system not detected (skipped)")
            .with_category("system"),
    ]
}

fn ctx_with_provider(provider: &str, api_key: Option<&str>, offline: bool) -> (DoctorContext, TempDir) {
    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::default();
    cfg.default_provider = Some(provider.to_string());
    cfg.api_key = api_key.map(str::to_string);
    let profile = Profile {
        name: "test".into(),
        root: tmp.path().to_path_buf(),
    };
    (
        DoctorContext {
            profile,
            config: cfg,
            offline,
        },
        tmp,
    )
}

#[tokio::test]
async fn provider_ping_succeeds_on_mock_endpoint() {
    let _guard = MOCKITO_LOCK.lock().unwrap();
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("GET", "/models")
        .with_status(200)
        .with_body("[]")
        .create_async()
        .await;

    let url = format!("{}/models", server.url());
    let check = ProviderPingCheck::with_endpoint(url);
    let (ctx, _tmp) = ctx_with_provider("openrouter", Some("test-key"), false);
    let result = check.run(&ctx).await;
    mock.assert_async().await;
    assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
}

#[tokio::test]
async fn provider_ping_fails_on_401() {
    let _guard = MOCKITO_LOCK.lock().unwrap();
    let mut server = mockito::Server::new_async().await;
    let _mock = server
        .mock("GET", "/models")
        .with_status(401)
        .with_body(r#"{"error":"unauthorized"}"#)
        .create_async()
        .await;

    let url = format!("{}/models", server.url());
    let check = ProviderPingCheck::with_endpoint(url);
    let (ctx, _tmp) = ctx_with_provider("openrouter", Some("bad-key"), false);
    let result = check.run(&ctx).await;
    assert_eq!(result.severity, Severity::Fail);
    assert!(result.message.contains("401"));
    assert!(result.hint.is_some());
}

#[tokio::test]
async fn provider_ping_skips_when_offline() {
    let check = ProviderPingCheck::default();
    let (ctx, _tmp) = ctx_with_provider("openrouter", Some("k"), true);
    let result = check.run(&ctx).await;
    assert_eq!(result.severity, Severity::Info);
}

#[test]
fn allowlist_check_warns_on_strict_empty_allowlist_via_pure_helper() {
    // Verifies the diagnose helper used by AllowlistCheck — strict-mode
    // wiring is covered by the unit tests inside `policy.rs`.
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("command_allowlist.toml");
    std::fs::write(&file, "commands = []\n").unwrap();
    assert_eq!(diagnose_allowlist(&file), AllowlistDiagnosis::Empty);
}

// ── Renderer snapshots ─────────────────────────────────────────────

#[test]
fn report_text_renders_correctly() {
    let rendered = render_text(&fixture_results(), false);
    insta::assert_snapshot!("doctor_text", rendered);
}

#[test]
fn report_brief_renders_correctly() {
    let rendered = render_brief(&fixture_results());
    insta::assert_snapshot!("doctor_brief", rendered);
}

#[test]
fn report_json_renders_correctly() {
    let rendered = render_json(&fixture_results());
    insta::assert_json_snapshot!("doctor_json", rendered);
}

#[test]
fn render_dispatches_to_text() {
    let s = render(&fixture_results(), DoctorFormat::Text);
    assert!(s.contains("RantaiClaw Doctor"));
}

#[test]
fn render_dispatches_to_brief() {
    let s = render(&fixture_results(), DoctorFormat::Brief);
    assert!(s.starts_with("doctor:"));
}

#[test]
fn render_dispatches_to_json() {
    let s = render(&fixture_results(), DoctorFormat::Json);
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert_eq!(v["summary"]["total"], 4);
}
