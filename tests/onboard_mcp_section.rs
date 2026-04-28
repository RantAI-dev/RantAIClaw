//! Integration tests for Wave 2E — MCP curated picker.
//!
//! Plan: `docs/superpowers/plans/2026-04-27-onboarding-depth-v2.md`,
//! step 2E.5.
//!
//! These run against the public crate API; they avoid `dialoguer`
//! prompts (interactive) and the OAuth listener (interactive smoke
//! test only). The only behaviours we exercise here are deterministic
//! and side-effect-bounded:
//!   * curated list invariants (no slug overlaps);
//!   * `validate_mcp_startup` happy + sad paths via inline shell
//!     fixtures;
//!   * `register_mcp` writes the right `[mcp_servers.<slug>]` block.

use std::collections::HashSet;
use std::time::Instant;

use rantaiclaw::config::Config;
use rantaiclaw::mcp::curated::{
    AuthMethod, CuratedMcpServer, OAuthProvider, AUTHED, NO_AUTH,
};
use rantaiclaw::mcp::setup::{register_mcp, validate_mcp_startup};

#[test]
fn curated_lists_have_no_overlapping_slugs() {
    let mut seen = HashSet::new();
    for s in NO_AUTH.iter().chain(AUTHED.iter()) {
        assert!(
            seen.insert(s.slug),
            "duplicate slug across curated MCP lists: {}",
            s.slug
        );
    }
    // Sanity floor — if either list shrinks below the spec, the test
    // catches the regression. Spec §"Section 5 — mcp (NEW)" calls for
    // 3 zero-auth + 6 authed.
    assert_eq!(NO_AUTH.len(), 3, "expected 3 zero-auth servers");
    assert_eq!(AUTHED.len(), 6, "expected 6 authed servers");
}

#[test]
fn curated_authed_covers_required_providers() {
    // Spec: Notion, Google Drive, Slack, Google Calendar, Gmail, GitHub.
    let must = ["notion", "google-drive", "slack", "google-calendar", "gmail", "github"];
    let actual: HashSet<&str> = AUTHED.iter().map(|s| s.slug).collect();
    for slug in must {
        assert!(actual.contains(slug), "missing curated server: {slug}");
    }
}

#[test]
fn oauth_providers_have_kebab_slugs() {
    for p in [
        OAuthProvider::GoogleDrive,
        OAuthProvider::GoogleCalendar,
        OAuthProvider::Gmail,
    ] {
        let slug = p.slug();
        assert!(slug.chars().all(|c| c.is_ascii_lowercase() || c == '-'));
    }
}

#[test]
fn register_mcp_writes_correct_config_block() {
    let mut config = Config::default();
    let server = AUTHED.iter().find(|s| s.slug == "notion").unwrap();
    register_mcp(
        &mut config,
        server,
        &[("NOTION_API_KEY".into(), "secret-abc".into())],
    )
    .expect("register_mcp");

    let entry = config
        .mcp_servers
        .get("notion")
        .expect("notion entry present");
    assert_eq!(entry.command, "npx");
    assert_eq!(entry.args.first().map(String::as_str), Some("-y"));
    assert_eq!(
        entry.env.get("NOTION_API_KEY").map(String::as_str),
        Some("secret-abc")
    );
}

#[test]
fn register_mcp_is_idempotent_on_re_register() {
    let mut config = Config::default();
    let server = AUTHED.iter().find(|s| s.slug == "github").unwrap();
    register_mcp(
        &mut config,
        server,
        &[("GITHUB_PERSONAL_ACCESS_TOKEN".into(), "old".into())],
    )
    .unwrap();
    register_mcp(
        &mut config,
        server,
        &[("GITHUB_PERSONAL_ACCESS_TOKEN".into(), "new".into())],
    )
    .unwrap();
    assert_eq!(config.mcp_servers.len(), 1);
    assert_eq!(
        config.mcp_servers["github"]
            .env
            .get("GITHUB_PERSONAL_ACCESS_TOKEN")
            .map(String::as_str),
        Some("new"),
    );
}

#[tokio::test]
async fn validate_mcp_startup_succeeds_for_well_behaved_server() {
    // A "well-behaved" server, for our purposes, prints a single line
    // on stdout and exits. We don't validate the JSON-RPC shape — only
    // that the binary spawned, the install_command worked, and stdio
    // was wired. `printf` ships everywhere we run CI.
    let server = CuratedMcpServer {
        slug: "test-good",
        display_name: "Test (good)",
        summary: "fixture",
        install_command: &[
            "sh",
            "-c",
            r#"printf '{"jsonrpc":"2.0","id":0,"result":{}}\n'; exit 0"#,
        ],
        auth: AuthMethod::None,
        env_vars: &[],
    };
    validate_mcp_startup(&server, &[])
        .await
        .expect("well-behaved server should validate");
}

#[tokio::test]
async fn validate_mcp_startup_times_out_for_silent_server() {
    // `sleep 30` writes nothing to stdout; the 5 s timeout should fire.
    // We additionally bound the wall-clock so a hung test fails CI loudly.
    let server = CuratedMcpServer {
        slug: "test-silent",
        display_name: "Test (silent)",
        summary: "fixture",
        install_command: &["sh", "-c", "sleep 30"],
        auth: AuthMethod::None,
        env_vars: &[],
    };
    let started = Instant::now();
    let err = validate_mcp_startup(&server, &[])
        .await
        .expect_err("silent server must time out");
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_secs() < 10,
        "validation took longer than expected: {elapsed:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("no response") || msg.contains("timed out") || msg.contains("within"),
        "unexpected error: {msg}"
    );
}
