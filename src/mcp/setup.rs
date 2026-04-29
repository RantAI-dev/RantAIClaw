//! Interactive curated MCP picker — invoked by `rantaiclaw onboard`
//! Section 5 and by `rantaiclaw setup mcp` once Wave 3 wires the
//! subcommand.
//!
//! Spec: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`
//! §"Section 5 — mcp (NEW)" + §"MCP discovery".
//!
//! Flow:
//! 1. Yes/no: install zero-auth servers (web-fetch, time, filesystem).
//! 2. `MultiSelect` over the curated authed list.
//! 3. Per-pick credential collection — masked input or OAuth.
//! 4. Spawn-and-validate (5 s `initialize` ack); skip + warn on failure.
//! 5. Append to `config.mcp_servers` and persist secrets to
//!    `<profile>/secrets/api_keys.toml`.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use dialoguer::{theme::ColorfulTheme, Confirm, MultiSelect, Password};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{info, warn};

use crate::config::schema::McpServerConfig;
use crate::config::Config;
use crate::profile::Profile;

use super::curated::{AuthMethod, CuratedMcpServer, AUTHED, NO_AUTH};
use super::oauth;

/// Hard cap on how long we wait for a freshly-spawned MCP server to
/// echo its `initialize` JSON-RPC response on stdout.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(5);

/// Top-level entry point. Errors here only fire on hard I/O failures
/// — individual server validation failures are swallowed (warn + skip).
pub async fn run_interactive(profile: &Profile, config: &mut Config) -> Result<()> {
    let theme = ColorfulTheme::default();

    // 1. Zero-auth bundle.
    let install_zero_auth = Confirm::with_theme(&theme)
        .with_prompt("Install zero-auth MCP servers? (web-fetch, time, filesystem)")
        .default(true)
        .interact()
        .context("read zero-auth confirmation")?;
    if install_zero_auth {
        for server in NO_AUTH {
            // Spawn-and-validate the binary first so the user finds out at
            // setup time (not at first agent run) when the install_command
            // isn't on PATH or the binary is broken. Skip + warn on
            // failure — same UX as the authed branch.
            match validate_mcp_startup(server, &[]).await {
                Ok(()) => {
                    register_mcp(config, server, &[])?;
                    info!("MCP server registered: {}", server.slug);
                    println!("  added {}", server.display_name);
                }
                Err(e) => {
                    warn!(slug = server.slug, error = %e, "skipping zero-auth MCP server");
                    eprintln!("  skipped {} ({e})", server.display_name);
                }
            }
        }
    }

    // 2. Authed multi-select.
    let labels: Vec<String> = AUTHED
        .iter()
        .map(|s| format!("{}  —  {}", s.display_name, s.summary))
        .collect();
    let picks = MultiSelect::with_theme(&theme)
        .with_prompt("Select MCP servers to add (space to toggle, enter to confirm)")
        .items(&labels)
        .interact()
        .context("read MCP multi-select")?;

    for idx in picks {
        let server = &AUTHED[idx];
        match collect_and_register(profile, config, server).await {
            Ok(()) => println!("  added {}", server.display_name),
            Err(e) => {
                warn!(slug = server.slug, error = %e, "skipping MCP server");
                eprintln!("  skipped {} ({e})", server.display_name);
            }
        }
    }

    Ok(())
}

/// Collect credentials, validate, persist secrets, register in config.
/// Bubbling an error short-circuits *only that server*; the caller
/// catches and warns.
async fn collect_and_register(
    profile: &Profile,
    config: &mut Config,
    server: &CuratedMcpServer,
) -> Result<()> {
    let theme = ColorfulTheme::default();
    let env_pairs: Vec<(String, String)> = match &server.auth {
        AuthMethod::None => Vec::new(),
        AuthMethod::Token { secret_key, hint } => {
            let prompt = format!("{} — paste {} ({})", server.display_name, secret_key, hint);
            let token = Password::with_theme(&theme)
                .with_prompt(prompt)
                .interact()
                .context("read masked token")?;
            vec![((*secret_key).to_string(), token)]
        }
        AuthMethod::OAuth { provider, scopes } => {
            let token = oauth::run_oauth(*provider, scopes).await?;
            let env_key = format!("{}_OAUTH_TOKEN", server.slug.replace('-', "_").to_uppercase());
            vec![(env_key, token)]
        }
    };

    validate_mcp_startup(server, &env_pairs).await?;
    register_mcp(config, server, &env_pairs)?;
    write_secrets(profile, &env_pairs)?;
    Ok(())
}

/// Append a curated server to `config.mcp_servers` keyed on its slug.
/// Idempotent — re-running overwrites the previous entry, which is the
/// right call when the user re-runs `setup mcp` to rotate creds.
pub fn register_mcp(
    config: &mut Config,
    server: &CuratedMcpServer,
    env: &[(String, String)],
) -> Result<()> {
    let (command, args) = server.split_command();
    if command.is_empty() {
        return Err(anyhow!(
            "curated server {} has empty install_command",
            server.slug
        ));
    }
    let env_map: HashMap<String, String> = env.iter().cloned().collect();
    let entry = McpServerConfig {
        command,
        args,
        env: env_map,
    };
    config.mcp_servers.insert(server.slug.to_string(), entry);
    Ok(())
}

/// Spawn the configured MCP server, send a single `initialize`
/// JSON-RPC request on stdin, wait up to [`VALIDATE_TIMEOUT`] for any
/// response on stdout, then kill it.
///
/// We don't parse the response strictly — any line at all means the
/// child binary exists, the install_command works, and the server
/// process accepted stdio. That's the bar this validation is gating.
pub async fn validate_mcp_startup(
    server: &CuratedMcpServer,
    env: &[(String, String)],
) -> Result<()> {
    let (cmd, args) = server.split_command();
    let mut child = Command::new(&cmd)
        .args(&args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {} for validation", server.slug))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("no stdin on validation child"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no stdout on validation child"))?;

    let init = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "rantaiclaw-setup", "version": env!("CARGO_PKG_VERSION")}
        }
    });
    stdin
        .write_all(format!("{init}\n").as_bytes())
        .await
        .context("write initialize request")?;
    stdin.flush().await.ok();

    let mut reader = BufReader::new(stdout).lines();
    let read_fut = reader.next_line();

    let outcome = tokio::time::timeout(VALIDATE_TIMEOUT, read_fut).await;
    // Best-effort cleanup regardless.
    let _ = child.kill().await;

    match outcome {
        Ok(Ok(Some(line))) if !line.trim().is_empty() => Ok(()),
        Ok(Ok(Some(_))) => Err(anyhow!("server emitted empty line, no JSON-RPC response")),
        Ok(Ok(None)) => Err(anyhow!("server closed stdout before responding")),
        Ok(Err(e)) => Err(anyhow!("read stdout: {e}")),
        Err(_) => Err(anyhow!(
            "no response within {}s",
            VALIDATE_TIMEOUT.as_secs()
        )),
    }
}

/// Append (or update) entries in `<profile>/secrets/api_keys.toml`.
///
/// Layout — flat top-level table:
/// ```toml
/// NOTION_API_KEY = "secret_…"
/// SLACK_BOT_TOKEN = "xoxb-…"
/// ```
///
/// Mode 0600 on Unix; secrets dir already 0700 from `ProfileManager::ensure`.
pub fn write_secrets(profile: &Profile, env: &[(String, String)]) -> Result<()> {
    if env.is_empty() {
        return Ok(());
    }
    let path = profile.secrets_dir().join("api_keys.toml");
    write_secrets_to(&path, env)
}

/// Path-injectable variant of [`write_secrets`] — used by tests so we
/// don't need a real `ProfileManager` tree.
pub fn write_secrets_to(path: &Path, env: &[(String, String)]) -> Result<()> {
    if env.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create secrets dir {}", parent.display()))?;
    }

    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml::Table = if existing.trim().is_empty() {
        toml::Table::new()
    } else {
        existing
            .parse()
            .with_context(|| format!("parse existing {}", path.display()))?
    };
    for (k, v) in env {
        doc.insert(k.clone(), toml::Value::String(v.clone()));
    }
    let serialised =
        toml::to_string_pretty(&doc).context("serialise secrets table")?;
    std::fs::write(path, serialised)
        .with_context(|| format!("write secrets file {}", path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn write_secrets_creates_and_merges() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("api_keys.toml");

        write_secrets_to(&path, &[("NOTION_API_KEY".into(), "secret-1".into())]).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("NOTION_API_KEY"));
        assert!(body.contains("secret-1"));

        // Merge: second key appends, first preserved.
        write_secrets_to(&path, &[("SLACK_BOT_TOKEN".into(), "xoxb-2".into())]).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("NOTION_API_KEY"));
        assert!(body.contains("SLACK_BOT_TOKEN"));
    }

    #[test]
    fn register_mcp_writes_correct_block() {
        let mut config = Config::default();
        let server = &super::AUTHED[0]; // notion
        register_mcp(
            &mut config,
            server,
            &[("NOTION_API_KEY".into(), "secret".into())],
        )
        .unwrap();
        let entry = config.mcp_servers.get("notion").unwrap();
        assert_eq!(entry.command, "npx");
        assert_eq!(entry.env.get("NOTION_API_KEY").map(String::as_str), Some("secret"));
    }
}
