//! Onboarding section 5 of 13 — MCP curated picker.
//!
//! Implements the stub `SetupSection` trait from `_stub.rs`; Wave 3
//! rewrites this file against the real (async, fuller-context) trait.
//!
//! All heavy lifting lives in `crate::mcp::setup` so the same picker
//! is reusable from `rantaiclaw setup mcp` once Wave 3 wires the
//! subcommand.

use anyhow::Result;

use super::_stub::{SetupContext, SetupSection};
use crate::config::Config;
use crate::mcp::curated::{curated_count, AUTHED, NO_AUTH};
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

    /// We treat the section as "configured" only if at least one of
    /// the curated servers has already been registered. Custom user-
    /// added MCP entries don't count — re-running the wizard should
    /// still offer the curated picker even when they exist.
    fn is_already_configured(&self, _profile: &Profile, config: &Config) -> bool {
        let curated_slugs: Vec<&str> = NO_AUTH
            .iter()
            .chain(AUTHED.iter())
            .map(|s| s.slug)
            .collect();
        config
            .mcp_servers
            .keys()
            .any(|k| curated_slugs.iter().any(|s| s == k))
    }

    fn run(&self, ctx: &mut SetupContext) -> Result<()> {
        if !ctx.interactive {
            print_headless_hint();
            return Ok(());
        }

        // The picker is async; the orchestrator (Wave 3) is async too,
        // but the stub trait is sync. Bridge here without taking an
        // opinion on the outer reactor.
        let result: Result<()> = match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| {
                handle.block_on(setup::run_interactive(ctx.profile, ctx.config))
            }),
            Err(_) => {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(setup::run_interactive(ctx.profile, ctx.config))
            }
        };
        result
    }

    fn headless_hint(&self) -> &'static str {
        "Run `rantaiclaw setup mcp` interactively to add curated MCP servers \
         (Notion, Google Drive, Slack, Calendar, Gmail, GitHub + zero-auth set)."
    }
}

fn print_headless_hint() {
    eprintln!(
        "skipping MCP section in headless mode — re-run interactively to install \
         from {} curated servers (zero-auth: {}, authed: {}).",
        curated_count(),
        NO_AUTH.len(),
        AUTHED.len(),
    );
    for server in NO_AUTH.iter().chain(AUTHED.iter()) {
        eprintln!("  rantaiclaw mcp add {}", server.slug);
    }
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

    #[test]
    fn is_already_configured_only_for_curated_slugs() {
        let section = McpSection;
        let mut config = Config::default();

        config.mcp_servers.insert(
            "my-custom-mcp".into(),
            McpServerConfig {
                command: "node".into(),
                args: vec!["./server.js".into()],
                env: Default::default(),
            },
        );
        let dummy_profile = Profile {
            name: "default".into(),
            root: std::path::PathBuf::from("/tmp"),
        };
        assert!(!section.is_already_configured(&dummy_profile, &config));

        config.mcp_servers.insert(
            "notion".into(),
            McpServerConfig {
                command: "npx".into(),
                args: vec![],
                env: Default::default(),
            },
        );
        assert!(section.is_already_configured(&dummy_profile, &config));
    }
}
