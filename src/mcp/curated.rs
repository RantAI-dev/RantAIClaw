//! Curated MCP server registry — servers offered by the onboarding
//! wizard's MCP section.
//!
//! Source-of-truth: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Section 5 — mcp (NEW)" + §"MCP discovery".
//!
//! ## Maintenance (2026-05-14 audit)
//!
//! Reference MCP servers are published either to npm under
//! `@modelcontextprotocol/server-*` or to PyPI under `mcp-server-*`
//! (run via `uvx`). The split is **per server, not per language** —
//! some Python-only, some TypeScript-only. The split has shifted at
//! least once; entries here must be checked against
//! https://github.com/modelcontextprotocol/servers before any
//! release.
//!
//! When an entry's package goes 404 or moves, fix it here; don't
//! ship a wizard that offers servers users can't install. Removed
//! entries (Google Calendar, Gmail) had no official MCP server in
//! the registry as of this audit — the reference servers list ends
//! at Drive for the Google suite.

#[derive(Debug, Clone, Copy)]
pub struct CuratedMcpServer {
    pub slug: &'static str,
    pub display_name: &'static str,
    pub summary: &'static str,
    pub install_command: &'static [&'static str],
    pub auth: AuthMethod,
    pub env_vars: &'static [&'static str],
}

impl CuratedMcpServer {
    pub fn split_command(&self) -> (String, Vec<String>) {
        match self.install_command.split_first() {
            Some((head, tail)) => (
                (*head).to_string(),
                tail.iter().map(|s| (*s).to_string()).collect(),
            ),
            None => (String::new(), Vec::new()),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum AuthMethod {
    None,
    Token {
        secret_key: &'static str,
        hint: &'static str,
    },
    OAuth {
        provider: OAuthProvider,
        scopes: &'static [&'static str],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthProvider {
    GoogleDrive,
    GoogleCalendar,
    Gmail,
}

impl OAuthProvider {
    pub fn slug(self) -> &'static str {
        match self {
            Self::GoogleDrive => "google-drive",
            Self::GoogleCalendar => "google-calendar",
            Self::Gmail => "gmail",
        }
    }
}

pub const NO_AUTH: &[CuratedMcpServer] = &[
    // Web Fetch is a Python reference server. Requires `uv` on PATH
    // (https://docs.astral.sh/uv/). If uv isn't installed, spawn
    // fails and `/mcp` shows the server as ✗ failed — clearer than
    // a misleading npm 404 we used to ship.
    CuratedMcpServer {
        slug: "web-fetch",
        display_name: "Web Fetch",
        summary: "HTTP GET arbitrary URLs and return text/markdown. (Requires `uv` installed; uvx provided.)",
        install_command: &["uvx", "mcp-server-fetch"],
        auth: AuthMethod::None,
        env_vars: &[],
    },
    // Time is also Python-only, same uvx requirement.
    CuratedMcpServer {
        slug: "time",
        display_name: "Time",
        summary: "Current time and timezone conversions. (Requires `uv` installed; uvx provided.)",
        install_command: &["uvx", "mcp-server-time"],
        auth: AuthMethod::None,
        env_vars: &[],
    },
    CuratedMcpServer {
        slug: "filesystem",
        display_name: "Filesystem",
        summary: "Sandboxed read/write inside the active workspace.",
        install_command: &["npx", "-y", "@modelcontextprotocol/server-filesystem"],
        auth: AuthMethod::None,
        env_vars: &[],
    },
];

pub const AUTHED: &[CuratedMcpServer] = &[
    // Notion's official MCP server moved from `@modelcontextprotocol/server-notion`
    // (404 as of 2026-05-14) to the Notion-published package.
    CuratedMcpServer {
        slug: "notion",
        display_name: "Notion",
        summary: "Read/search/append Notion pages and databases.",
        install_command: &["npx", "-y", "@notionhq/notion-mcp-server"],
        auth: AuthMethod::Token {
            secret_key: "NOTION_API_KEY",
            hint: "Internal integration token — notion.so/profile/integrations",
        },
        env_vars: &["NOTION_API_KEY"],
    },
    CuratedMcpServer {
        slug: "slack",
        display_name: "Slack",
        summary: "Post to channels, search history, manage threads.",
        install_command: &["npx", "-y", "@modelcontextprotocol/server-slack"],
        auth: AuthMethod::Token {
            secret_key: "SLACK_BOT_TOKEN",
            hint: "Bot token (xoxb-…) — api.slack.com/apps",
        },
        env_vars: &["SLACK_BOT_TOKEN"],
    },
    CuratedMcpServer {
        slug: "github",
        display_name: "GitHub",
        summary: "Issues, PRs, code search across user-accessible repos.",
        install_command: &["npx", "-y", "@modelcontextprotocol/server-github"],
        auth: AuthMethod::Token {
            secret_key: "GITHUB_PERSONAL_ACCESS_TOKEN",
            hint: "Fine-grained PAT — github.com/settings/tokens",
        },
        env_vars: &["GITHUB_PERSONAL_ACCESS_TOKEN"],
    },
    CuratedMcpServer {
        slug: "google-drive",
        display_name: "Google Drive",
        summary: "List, read, search files in Drive.",
        install_command: &["npx", "-y", "@modelcontextprotocol/server-gdrive"],
        auth: AuthMethod::OAuth {
            provider: OAuthProvider::GoogleDrive,
            scopes: &["https://www.googleapis.com/auth/drive.readonly"],
        },
        env_vars: &["GOOGLE_DRIVE_OAUTH_TOKEN"],
    },
    // Removed 2026-05-14: Google Calendar + Gmail. The
    // `@modelcontextprotocol/server-gcalendar` and
    // `@modelcontextprotocol/server-gmail` packages 404 on npm
    // and no replacement official server exists in the
    // modelcontextprotocol/servers registry. `OAuthProvider`
    // variants for them are kept so existing `oauth.rs` state in
    // the wild doesn't crash; they'll be re-added once an official
    // MCP server ships for either.
];

pub const fn curated_count() -> usize {
    NO_AUTH.len() + AUTHED.len()
}

pub fn find_by_slug(slug: &str) -> Option<&'static CuratedMcpServer> {
    NO_AUTH.iter().chain(AUTHED.iter()).find(|s| s.slug == slug)
}

#[cfg(test)]
mod unit {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn no_overlapping_slugs_internal() {
        let mut seen = HashSet::new();
        for s in NO_AUTH.iter().chain(AUTHED.iter()) {
            assert!(seen.insert(s.slug), "duplicate slug: {}", s.slug);
        }
    }

    #[test]
    fn count_matches_constant() {
        assert_eq!(curated_count(), NO_AUTH.len() + AUTHED.len());
    }

    #[test]
    fn split_command_extracts_program_and_args() {
        let entry = &NO_AUTH[0];
        let (cmd, args) = entry.split_command();
        // After 2026-05-14 audit, web-fetch uses the Python
        // reference server via uvx.
        assert_eq!(cmd, "uvx");
        assert_eq!(args, vec!["mcp-server-fetch"]);
    }

    #[test]
    fn all_known_npm_packages_are_under_a_real_scope() {
        // Guards against regressions to the pre-2026-05-14 list
        // that pointed at npm packages which returned 404. Any
        // npx entry must live under a known-published scope.
        for s in NO_AUTH.iter().chain(AUTHED.iter()) {
            let (cmd, args) = s.split_command();
            if cmd != "npx" {
                continue;
            }
            // Find the package arg (last token starting with @
            // or non-flag).
            let pkg = args
                .iter()
                .find(|a| a.starts_with('@') || !a.starts_with('-'))
                .unwrap_or_else(|| panic!("server {} has no package arg", s.slug));
            let known_good_prefixes = [
                "@modelcontextprotocol/server-filesystem",
                "@modelcontextprotocol/server-slack",
                "@modelcontextprotocol/server-github",
                "@modelcontextprotocol/server-gdrive",
                "@notionhq/notion-mcp-server",
            ];
            assert!(
                known_good_prefixes.iter().any(|p| pkg.starts_with(p)),
                "server {} uses unknown npm package: {}",
                s.slug,
                pkg
            );
        }
    }

    #[test]
    fn find_by_slug_resolves_zero_auth_and_authed() {
        assert!(find_by_slug("time").is_some());
        assert!(find_by_slug("notion").is_some());
        assert!(find_by_slug("nonexistent-slug").is_none());
    }
}
