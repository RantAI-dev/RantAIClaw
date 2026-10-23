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

    fn headless_hint(&self) -> &'static str {
        "Run `rantaiclaw setup mcp` interactively to add curated MCP servers \
         (Notion, Google Drive, Slack, Calendar, Gmail, GitHub + zero-auth set)."
    }
}

/// The hint the MCP section records when it could not prompt.
///
/// `SetupSection::headless_hint()` is a `&'static str` and cannot carry the
/// per-server commands, so the section builds the full hint here: the trait's
/// base text, then one `rantaiclaw mcp add <slug>` line per curated server.
/// The orchestrator prints the result once, with the other blocked sections.
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
        let _ = write!(hint, "\nrantaiclaw mcp add {}", server.slug);
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
    /// records must still name the commands that would set the section up.
    /// The trait's `headless_hint()` is a `&'static str` and cannot carry
    /// them, so the section builds the full hint itself.
    #[test]
    fn a_blocked_mcp_section_records_the_commands_that_would_set_it_up() {
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
            blocked.hint.contains("rantaiclaw mcp add"),
            "the recorded hint must name the command that adds a server: {}",
            blocked.hint
        );
        for server in NO_AUTH.iter().chain(AUTHED.iter()) {
            assert!(
                blocked
                    .hint
                    .contains(&format!("rantaiclaw mcp add {}", server.slug)),
                "the recorded hint must name every curated server: {}",
                blocked.hint
            );
        }
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
                env: std::collections::HashMap::default(),
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
                env: std::collections::HashMap::default(),
            },
        );
        assert!(section.is_already_configured(&dummy_profile, &config));
    }
}
