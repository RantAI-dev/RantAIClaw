//! Curated MCP server registry — the 9 servers offered by the v0.5.0
//! onboarding wizard's MCP section.
//!
//! Source-of-truth: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Section 5 — mcp (NEW)" + §"MCP discovery".

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
    CuratedMcpServer {
        slug: "web-fetch",
        display_name: "Web Fetch",
        summary: "HTTP GET arbitrary URLs and return text/markdown.",
        install_command: &["npx", "-y", "@modelcontextprotocol/server-fetch"],
        auth: AuthMethod::None,
        env_vars: &[],
    },
    CuratedMcpServer {
        slug: "time",
        display_name: "Time",
        summary: "Current time and timezone conversions.",
        install_command: &["npx", "-y", "@modelcontextprotocol/server-time"],
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
    CuratedMcpServer {
        slug: "notion",
        display_name: "Notion",
        summary: "Read/search/append Notion pages and databases.",
        install_command: &["npx", "-y", "@modelcontextprotocol/server-notion"],
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
    CuratedMcpServer {
        slug: "google-calendar",
        display_name: "Google Calendar",
        summary: "Read events, create reminders, RSVP.",
        install_command: &["npx", "-y", "@modelcontextprotocol/server-gcalendar"],
        auth: AuthMethod::OAuth {
            provider: OAuthProvider::GoogleCalendar,
            scopes: &["https://www.googleapis.com/auth/calendar"],
        },
        env_vars: &["GOOGLE_CALENDAR_OAUTH_TOKEN"],
    },
    CuratedMcpServer {
        slug: "gmail",
        display_name: "Gmail",
        summary: "Read inbox, draft + send messages.",
        install_command: &["npx", "-y", "@modelcontextprotocol/server-gmail"],
        auth: AuthMethod::OAuth {
            provider: OAuthProvider::Gmail,
            scopes: &[
                "https://www.googleapis.com/auth/gmail.readonly",
                "https://www.googleapis.com/auth/gmail.send",
            ],
        },
        env_vars: &["GMAIL_OAUTH_TOKEN"],
    },
];

pub const fn curated_count() -> usize {
    NO_AUTH.len() + AUTHED.len()
}

pub fn find_by_slug(slug: &str) -> Option<&'static CuratedMcpServer> {
    NO_AUTH
        .iter()
        .chain(AUTHED.iter())
        .find(|s| s.slug == slug)
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
        assert_eq!(cmd, "npx");
        assert_eq!(args, vec!["-y", "@modelcontextprotocol/server-fetch"]);
    }

    #[test]
    fn find_by_slug_resolves_zero_auth_and_authed() {
        assert!(find_by_slug("time").is_some());
        assert!(find_by_slug("notion").is_some());
        assert!(find_by_slug("nonexistent-slug").is_none());
    }
}
