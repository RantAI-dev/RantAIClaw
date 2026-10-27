//! Onboarding section 5 of 13 — MCP curated picker.
//!
//! Implements the stub `SetupSection` trait from `_stub.rs`; Wave 3
//! rewrites this file against the real (async, fuller-context) trait.
//!
//! All heavy lifting lives in `crate::mcp::setup` so the same picker
//! is reusable from `rantaiclaw setup mcp` once Wave 3 wires the
//! subcommand.

use std::fmt::Write as _;

use anyhow::Result;

use super::{SetupContext, SetupSection};
use crate::config::Config;
use crate::mcp::curated::{curated_count, find_by_slug, AUTHED, NO_AUTH};
use crate::mcp::setup;
use crate::profile::Profile;

pub struct McpSection;

impl SetupSection for McpSection {
    fn name(&self) -> &'static str {
        "mcp"
    }

    fn description(&self) -> &'static str {
        "Add curated MCP servers (zero-auth + token + OAuth)."
    }

    /// We treat the section as "configured" only if at least one curated
    /// server is registered **with its credential**. An authed server whose
    /// env is empty is the artifact of a setup that claimed auth without a
    /// finished flow; it must not silence the picker, or the login can
    /// never actually happen. Custom user-added MCP entries never count —
    /// re-running the wizard should still offer the curated picker.
    fn is_already_configured(&self, _profile: &Profile, config: &Config) -> bool {
        config
            .mcp_servers
            .iter()
            .any(|(slug, entry)| match find_by_slug(slug) {
                // A zero-auth server needs nothing beyond being registered.
                Some(server) if server.env_vars.is_empty() => true,
                Some(server) => server.env_vars.iter().any(|key| {
                    entry
                        .env
                        .get(*key)
                        .is_some_and(|value| !value.trim().is_empty())
                }),
                None => false,
            })
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        if !ctx.interactive {
            ctx.record_blocked(self.name(), &blocked_hint(self.headless_hint()));
            return Ok(());
        }

        // The picker is async; the orchestrator (Wave 3) is async too,
        // but the stub trait is sync. Bridge here without taking an
        // opinion on the outer reactor.
        let result: Result<()> = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                tokio::task::block_in_place(|| handle.block_on(setup::run_interactive(ctx.config)))
            }
            Err(_) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(setup::run_interactive(ctx.config))
            }
        };
        result
    }

    /// The base text names the one command that finishes the setup, and no
    /// server: the curated list is built from the registry below, so a
    /// name written here goes stale the moment a server is dropped — this
    /// text still named Calendar and Gmail, removed from the curated list
    /// in v0.6.51.
    fn headless_hint(&self) -> &'static str {
        "Run `rantaiclaw setup mcp` interactively to add curated MCP servers."
    }
}

/// The hint the MCP section records when it could not prompt.
///
/// `SetupSection::headless_hint()` is a `&'static str` and cannot carry the
/// server list, so the section builds the full hint here: the trait's base
/// text — which names the one command that finishes this setup — followed by
/// the curated servers the picker would offer. There is deliberately no
/// per-server command: the picker is the only surface that collects
/// credentials, so the hint may only name `rantaiclaw setup mcp`.
fn blocked_hint(base: &str) -> String {
    let mut hint = String::new();
    hint.push_str(base);
    hint.push('\n');
    let _ = write!(
        hint,
        "Curated servers ({} total: {} zero-auth, {} needing auth):",
        curated_count(),
        NO_AUTH.len(),
        AUTHED.len(),
    );
    for server in NO_AUTH.iter().chain(AUTHED.iter()) {
        let _ = write!(hint, "\n  {} — {}", server.slug, server.display_name);
    }
    hint
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::McpServerConfig;

    #[test]
    fn name_and_description_are_set() {
        let section = McpSection;
        assert_eq!(section.name(), "mcp");
        assert!(!section.description().is_empty());
        assert!(!section.headless_hint().is_empty());
    }

    /// A headless MCP section records itself as blocked, and the hint it
    /// records must name a command that exists and finishes the setup — the
    /// picker, not a per-server command that was never wired up.
    #[test]
    fn a_blocked_mcp_section_names_the_command_that_finishes_the_setup() {
        let section = McpSection;
        let mut config = Config::default();
        let profile = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp"),
        };
        let mut ctx = SetupContext {
            profile: &profile,
            config: &mut config,
            interactive: false,
            force: false,
            blocked: Vec::new(),
        };

        section.run(&mut ctx).unwrap();

        assert_eq!(
            ctx.blocked.len(),
            1,
            "a headless MCP section records itself as blocked"
        );
        let blocked = &ctx.blocked[0];
        assert_eq!(blocked.name, "mcp");
        assert!(
            blocked.hint.contains(section.headless_hint()),
            "the recorded hint keeps the trait's base hint: {}",
            blocked.hint
        );
        assert!(
            blocked.hint.contains("rantaiclaw setup mcp"),
            "the recorded hint must name the command that finishes the setup: {}",
            blocked.hint
        );
        assert!(
            !blocked.hint.contains("mcp add"),
            "no hint may name a command that does not exist: {}",
            blocked.hint
        );
        for server in NO_AUTH.iter().chain(AUTHED.iter()) {
            assert!(
                blocked.hint.contains(server.slug),
                "the recorded hint still lists every curated server: {}",
                blocked.hint
            );
        }
    }

    #[test]
    fn custom_servers_do_not_count_as_configured() {
        let section = McpSection;
        let mut config = Config::default();

        config.mcp_servers.insert(
            "my-custom-mcp".into(),
            McpServerConfig {
                command: "node".into(),
                args: vec!["./server.js".into()],
                env: std::collections::HashMap::default(),
            },
        );
        let dummy_profile = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp"),
        };
        assert!(!section.is_already_configured(&dummy_profile, &config));
    }

    /// An authed server counts as configured only once its credential was
    /// actually obtained: the tokenless entries a claiming setup used to
    /// write must not silence the picker.
    #[test]
    fn an_authed_server_without_its_credential_does_not_count_as_configured() {
        let section = McpSection;
        let mut config = Config::default();
        let dummy_profile = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp"),
        };

        let tokenless = |slug: &str| {
            (
                slug.to_string(),
                McpServerConfig {
                    command: "npx".into(),
                    args: vec![],
                    env: std::collections::HashMap::default(),
                },
            )
        };
        let (notion_key, notion_entry) = tokenless("notion");
        config.mcp_servers.insert(notion_key, notion_entry);
        let (drive_key, drive_entry) = tokenless("google-drive");
        config.mcp_servers.insert(drive_key, drive_entry);
        assert!(
            !section.is_already_configured(&dummy_profile, &config),
            "tokenless authed entries are not a finished setup"
        );

        // A zero-auth server still counts by being present — it needs no
        // credential.
        config.mcp_servers.insert(
            "web-fetch".into(),
            McpServerConfig {
                command: "uvx".into(),
                args: vec![],
                env: std::collections::HashMap::default(),
            },
        );
        assert!(section.is_already_configured(&dummy_profile, &config));
    }

    #[test]
    fn an_authed_server_with_its_credential_counts_as_configured() {
        let section = McpSection;
        let mut config = Config::default();
        let dummy_profile = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp"),
        };

        let mut env = std::collections::HashMap::default();
        env.insert("NOTION_API_KEY".to_string(), "secret".to_string());
        config.mcp_servers.insert(
            "notion".into(),
            McpServerConfig {
                command: "npx".into(),
                args: vec![],
                env,
            },
        );
        assert!(section.is_already_configured(&dummy_profile, &config));

        // The credential key must hold a value — a present-but-empty key is
        // the same unfinished state as no key at all.
        let mut empty_value = std::collections::HashMap::default();
        empty_value.insert("NOTION_API_KEY".to_string(), String::new());
        config.mcp_servers.insert(
            "notion".into(),
            McpServerConfig {
                command: "npx".into(),
                args: vec![],
                env: empty_value,
            },
        );
        assert!(!section.is_already_configured(&dummy_profile, &config));
    }
}
