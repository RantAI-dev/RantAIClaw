//! russh-backed SSH session: connect (password/key), exec, and SFTP push/pull.
//! Server keys are verified trust-on-first-use against `~/.rantaiclaw/ssh_known_hosts.json`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use russh::client::{Config, Handle, Handler};
use russh::ChannelMsg;
use russh_keys::key;

use super::registry;

/// Authentication material supplied to [`connect`].
#[derive(Debug, Clone)]
pub enum Auth {
    /// Password authentication.
    Password(String),
    /// Public-key authentication from a file path or inline PEM, with optional passphrase.
    Key {
        path: Option<String>,
        pem: Option<String>,
        passphrase: Option<String>,
    },
    /// ssh-agent (not yet implemented).
    Agent,
}

/// Result of a remote command execution.
#[derive(Debug, Clone)]
pub struct ExecOut {
    /// Remote exit code, or -1 if the server sent no exit status.
    pub code: i64,
    pub stdout: String,
    pub stderr: String,
}

/// A live SSH connection. `channel_open_session` is `&self`, so concurrent
/// exec/sftp calls share one connection without locking.
pub struct SshConn {
    pub id: String,
    handle: Handle<ClientHandler>,
}

/// Build the canonical session id.
#[must_use]
pub fn session_id(user: &str, host: &str, port: u16) -> String {
    format!("{user}@{host}:{port}")
}

/// Connect and authenticate, storing the session in the registry. Returns the session id.
///
/// # Errors
/// Returns an error if the TCP/SSH handshake fails, authentication is rejected,
/// or key material cannot be loaded.
pub async fn connect(host: &str, port: u16, user: &str, auth: Auth) -> Result<String> {
    let config = Arc::new(Config::default());
    let handler = ClientHandler {
        endpoint: format!("{host}:{port}"),
    };
    let mut handle = russh::client::connect(config, (host, port), handler)
        .await
        .map_err(|e| anyhow!("ssh connect to {host}:{port} failed: {e}"))?;

    let authed = authenticate(&mut handle, user, auth).await?;
    if !authed {
        bail!("ssh authentication failed for {user}@{host}:{port}");
    }

    let id = session_id(user, host, port);
    registry::insert(id.clone(), Arc::new(SshConn { id: id.clone(), handle })).await;
    Ok(id)
}

async fn authenticate(handle: &mut Handle<ClientHandler>, user: &str, auth: Auth) -> Result<bool> {
    match auth {
        Auth::Password(pw) => Ok(handle.authenticate_password(user, pw).await?),
        Auth::Key {
            path,
            pem,
            passphrase,
        } => {
            let keypair = if let Some(pem) = pem {
                russh_keys::decode_secret_key(&pem, passphrase.as_deref())
                    .map_err(|e| anyhow!("invalid private key (pem): {e}"))?
            } else if let Some(path) = path {
                russh_keys::load_secret_key(&path, passphrase.as_deref())
                    .map_err(|e| anyhow!("cannot load key {path}: {e}"))?
            } else {
                bail!("key auth requires key_path or key_pem");
            };
            Ok(handle
                .authenticate_publickey(user, Arc::new(keypair))
                .await?)
        }
        Auth::Agent => bail!("ssh-agent auth is not yet supported; use password or key"),
    }
}

/// Run a command on a session, capturing stdout/stderr and the exit code.
///
/// # Errors
/// Returns an error if the session id is unknown, a channel cannot be opened,
/// or the command exceeds `timeout_secs`.
pub async fn exec(id: &str, command: &str, timeout_secs: u64) -> Result<ExecOut> {
    let conn = registry::get(id)
        .await
        .ok_or_else(|| anyhow!("no ssh session `{id}` (connect first)"))?;
    let command = command.to_string();
    let fut = async move {
        let mut channel = conn.handle.channel_open_session().await?;
        channel.exec(true, command.as_bytes()).await?;
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut code: Option<u32> = None;
        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, ext } => {
                    if ext == 1 {
                        stderr.extend_from_slice(&data);
                    } else {
                        stdout.extend_from_slice(&data);
                    }
                }
                ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                _ => {}
            }
        }
        Ok::<ExecOut, anyhow::Error>(ExecOut {
            code: code.map_or(-1, i64::from),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    };
    tokio::time::timeout(Duration::from_secs(timeout_secs), fut)
        .await
        .map_err(|_| anyhow!("exec timed out after {timeout_secs}s"))?
}

/// Upload a local file to the remote host over SFTP.
///
/// # Errors
/// Returns an error if the session is unknown, the local file cannot be read,
/// or the SFTP transfer fails.
pub async fn push(id: &str, local: &str, remote: &str) -> Result<()> {
    let conn = registry::get(id)
        .await
        .ok_or_else(|| anyhow!("no ssh session `{id}`"))?;
    let data = tokio::fs::read(local)
        .await
        .map_err(|e| anyhow!("cannot read local file {local}: {e}"))?;
    let sftp = open_sftp(&conn).await?;
    sftp.write(remote, &data)
        .await
        .map_err(|e| anyhow!("sftp upload to {remote} failed: {e}"))?;
    Ok(())
}

/// Download a remote file to a local path over SFTP.
///
/// # Errors
/// Returns an error if the session is unknown or the SFTP transfer fails.
pub async fn pull(id: &str, remote: &str, local: &str) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let conn = registry::get(id)
        .await
        .ok_or_else(|| anyhow!("no ssh session `{id}`"))?;
    let sftp = open_sftp(&conn).await?;
    let mut file = sftp
        .open(remote)
        .await
        .map_err(|e| anyhow!("sftp open {remote} failed: {e}"))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)
        .await
        .map_err(|e| anyhow!("sftp read {remote} failed: {e}"))?;
    tokio::fs::write(local, &buf)
        .await
        .map_err(|e| anyhow!("cannot write local file {local}: {e}"))?;
    Ok(())
}

async fn open_sftp(conn: &SshConn) -> Result<russh_sftp::client::SftpSession> {
    let channel = conn.handle.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| anyhow!("sftp subsystem failed: {e}"))
}

/// Close and forget a session.
pub async fn disconnect(id: &str) -> bool {
    registry::remove(id).await
}

// --- server-key verification (TOFU) ---

struct ClientHandler {
    endpoint: String,
}

#[async_trait]
impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(tofu_accept(&self.endpoint, &server_public_key.fingerprint()))
    }
}

fn known_hosts_path() -> PathBuf {
    crate::profile::paths::rantaiclaw_root().join("ssh_known_hosts.json")
}

/// Trust-on-first-use: accept an unseen host (recording its key), accept a host
/// whose key matches the record, reject a host whose key changed (MITM guard).
fn tofu_accept(endpoint: &str, key_id: &str) -> bool {
    let path = known_hosts_path();
    let mut map: HashMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    match map.get(endpoint) {
        Some(known) if known == key_id => true,
        Some(_) => false,
        None => {
            map.insert(endpoint.to_string(), key_id.to_string());
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(s) = serde_json::to_string_pretty(&map) {
                let _ = std::fs::write(&path, s);
            }
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_format() {
        assert_eq!(session_id("root", "10.0.0.5", 22), "root@10.0.0.5:22");
        assert_eq!(session_id("ubuntu", "host.local", 2222), "ubuntu@host.local:2222");
    }
}
