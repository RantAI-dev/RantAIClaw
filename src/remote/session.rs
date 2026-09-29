//! russh-backed SSH session: connect (password/key), exec, and SFTP push/pull.
//! Server keys are verified trust-on-first-use against `~/.rantaiclaw/ssh_known_hosts.json`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use russh::client::{Config, Handle, Handler};
use russh::keys::{Algorithm, EcdsaCurve, HashAlg, PrivateKeyWithHashAlg, PublicKey};
use russh::{kex, ChannelMsg, Preferred};

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
    let config = Arc::new(client_config());
    let handler = ClientHandler {
        endpoint: format!("{host}:{port}"),
        known_hosts: known_hosts_path(),
    };
    let mut handle = russh::client::connect(config, (host, port), handler)
        .await
        .map_err(|e| anyhow!("ssh connect to {host}:{port} failed: {e}"))?;

    let authed = authenticate(&mut handle, user, auth).await?;
    if !authed {
        bail!("ssh authentication failed for {user}@{host}:{port}");
    }

    let id = session_id(user, host, port);
    registry::insert(
        id.clone(),
        Arc::new(SshConn {
            id: id.clone(),
            handle,
        }),
    )
    .await;
    Ok(id)
}

/// The client configuration: russh's defaults with the algorithm lists pinned.
fn client_config() -> Config {
    Config {
        preferred: preferred_algorithms(),
        ..Config::default()
    }
}

/// The algorithm lists russh 0.45 offered by default, minus the entries that 0.60
/// dropped from its own defaults (the SHA-1 MACs). russh 0.60 additionally offers
/// `ssh-rsa` (SHA-1) host keys, `ecdsa-sha2-nistp384` and extra key exchanges;
/// none of those were enabled before, so they stay off here.
fn preferred_algorithms() -> Preferred {
    Preferred {
        kex: Cow::Borrowed(&[
            kex::CURVE25519,
            kex::CURVE25519_PRE_RFC_8731,
            kex::DH_G16_SHA512,
            kex::DH_G14_SHA256,
            kex::EXTENSION_SUPPORT_AS_CLIENT,
            kex::EXTENSION_SUPPORT_AS_SERVER,
            kex::EXTENSION_OPENSSH_STRICT_KEX_AS_CLIENT,
            kex::EXTENSION_OPENSSH_STRICT_KEX_AS_SERVER,
        ]),
        key: Cow::Borrowed(&[
            Algorithm::Ed25519,
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP256,
            },
            Algorithm::Ecdsa {
                curve: EcdsaCurve::NistP521,
            },
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha256),
            },
            Algorithm::Rsa {
                hash: Some(HashAlg::Sha512),
            },
        ]),
        ..Preferred::default()
    }
}

/// Pick the RSA signature hash from the best SHA-2 hash the server advertised in
/// `server-sig-algs`. `None` covers both "no such extension" and "only the SHA-1
/// `ssh-rsa` listed"; either way the key signs with SHA-256, never SHA-1.
fn rsa_signing_hash(advertised: Option<HashAlg>) -> Option<HashAlg> {
    advertised.or(Some(HashAlg::Sha256))
}

async fn authenticate(handle: &mut Handle<ClientHandler>, user: &str, auth: Auth) -> Result<bool> {
    match auth {
        Auth::Password(pw) => Ok(handle.authenticate_password(user, pw).await?.success()),
        Auth::Key {
            path,
            pem,
            passphrase,
        } => {
            let keypair = if let Some(pem) = pem {
                russh::keys::decode_secret_key(&pem, passphrase.as_deref())
                    .map_err(|e| anyhow!("invalid private key (pem): {e}"))?
            } else if let Some(path) = path {
                russh::keys::load_secret_key(&path, passphrase.as_deref())
                    .map_err(|e| anyhow!("cannot load key {path}: {e}"))?
            } else {
                bail!("key auth requires key_path or key_pem");
            };
            // RSA keys sign with the strongest SHA-2 hash the server accepts, and
            // never fall back to the legacy SHA-1 `ssh-rsa` (`None` would mean that).
            // Other key types have no hash choice, so skip the up-to-1s wait for it.
            let hash_alg = if keypair.algorithm().is_rsa() {
                rsa_signing_hash(handle.best_supported_rsa_hash().await?.flatten())
            } else {
                None
            };
            let key = PrivateKeyWithHashAlg::new(Arc::new(keypair), hash_alg);
            Ok(handle.authenticate_publickey(user, key).await?.success())
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

    // Open the channel OUTSIDE the timeout so that, on timeout, we still hold it
    // and can EOF+close it — freeing the channel on both ends — instead of
    // silently abandoning it. NOTE: OpenSSH commonly ignores signals on non-pty
    // exec channels, so this reclaims resources but does NOT guarantee the remote
    // command stops; the `pty`/tmux path is the answer when that's required.
    let mut channel = match conn.handle.channel_open_session().await {
        Ok(channel) => channel,
        Err(e) => {
            // A failed channel-open means the transport is gone — evict the corpse
            // so a later exec gets a clean "reconnect" instead of hitting it again.
            registry::remove(id).await;
            return Err(anyhow!("ssh session `{id}` is dead ({e}); reconnect"));
        }
    };
    channel.exec(true, command.as_bytes()).await?;

    let collect = async {
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
        ExecOut {
            code: code.map_or(-1, i64::from),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        }
    };

    match tokio::time::timeout(Duration::from_secs(timeout_secs), collect).await {
        Ok(out) => Ok(out),
        Err(_) => {
            let _ = channel.eof().await;
            let _ = channel.close().await;
            Err(anyhow!(
                "exec timed out after {timeout_secs}s (channel closed)"
            ))
        }
    }
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
    known_hosts: PathBuf,
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(tofu_accept(
            &self.known_hosts,
            &self.endpoint,
            &host_key_id(server_public_key),
        ))
    }
}

/// The string stored in `ssh_known_hosts.json` for a server key: the unpadded
/// base64 SHA-256 of the key's SSH wire encoding, i.e. `ssh-keygen -l` without
/// the `SHA256:` prefix. Changing it invalidates every stored entry.
fn host_key_id(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256)
        .to_string()
        .trim_start_matches("SHA256:")
        .to_string()
}

fn known_hosts_path() -> PathBuf {
    crate::profile::paths::rantaiclaw_root().join("ssh_known_hosts.json")
}

/// Trust-on-first-use: accept an unseen host (recording its key), accept a host
/// whose key matches the record, reject a host whose key changed (MITM guard).
fn tofu_accept(path: &Path, endpoint: &str, key_id: &str) -> bool {
    let mut map: HashMap<String, String> = std::fs::read_to_string(path)
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
                let _ = std::fs::write(path, s);
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
        assert_eq!(
            session_id("ubuntu", "host.local", 2222),
            "ubuntu@host.local:2222"
        );
    }

    // Public keys and their expected ids come from `ssh-keygen -t <type> -N ''` and
    // `ssh-keygen -lf <key>.pub` (the id is the printed fingerprint without `SHA256:`).
    // They were recorded on russh 0.45 and must not change: they are the strings
    // already stored in users' `ssh_known_hosts.json`.
    const ED25519_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIEEcvn5w1l/W/twZDdtWpDS0hEoMDkY5Rxn440cbm78W";
    const ED25519_A_ID: &str = "t+tQtoBpkopaZG4nmKH3LhbPAH0oQyMUJQLvj389utI";
    const ED25519_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIGCosfT+9wcj1e75a25OURJhyd2SMjVhcyFpVXCiVP0/";
    const ED25519_B_ID: &str = "pXexpCQatJS9cihHTaiZ+HjGQPiQOtLMU+Tl60Xxkvk";
    const RSA_2048: &str = "AAAAB3NzaC1yc2EAAAADAQABAAABAQCrYlv1P9MFwyxx6+kMZ96EECiS73elcbzDbeHIyI7sHf3h3CzQfPUdq1iYinXPOiab2BNxybLUh9RLMdzgdLj/UqLzED3dZ483Lae2+gQUrXxmH+9RGhr4MHI+xa+At3+8wnSoHFPyrNSGxYkm5wi3jTn2H2Y6EMvToQeKIgPOZ2ijWZDLpeLeBEb5Z2BM6cPHyHcmtr4qTxiowASIcOkQAKoMHAg38K/M51/ZicZ+oWdtHK1sY/6RvSjCWArs9ja0oTigHYsuCOTujinSJEQCmdClNjo9dbnw+ubpUuehd2sGU5LuaZgDul64y2udsz9xy0UJffkFfjfo1oZ4k2X5";
    const RSA_2048_ID: &str = "BLsRw3xM5DOcoEum/fjbP8+07W0+Ps3HLetZeaPgNBw";

    fn parse(b64: &str) -> PublicKey {
        russh::keys::parse_public_key_base64(b64).expect("test key parses")
    }

    #[test]
    fn host_key_id_matches_stored_known_hosts_format() {
        assert_eq!(host_key_id(&parse(ED25519_A)), ED25519_A_ID);
        assert_eq!(host_key_id(&parse(ED25519_B)), ED25519_B_ID);
        assert_eq!(host_key_id(&parse(RSA_2048)), RSA_2048_ID);
    }

    // The wire names below are the ones russh 0.45 offered by default. russh 0.60
    // widened its defaults (SHA-1 `ssh-rsa`, nistp384, extra key exchanges) and
    // dropped the SHA-1 MACs; this pins that only the old set is offered.
    #[test]
    fn preferred_algorithms_match_the_set_offered_before_the_russh_0_60_bump() {
        let preferred = client_config().preferred;
        let key: Vec<&str> = preferred.key.iter().map(AsRef::as_ref).collect();
        assert_eq!(
            key,
            [
                "ssh-ed25519",
                "ecdsa-sha2-nistp256",
                "ecdsa-sha2-nistp521",
                "rsa-sha2-256",
                "rsa-sha2-512"
            ]
        );
        let kex: Vec<&str> = preferred.kex.iter().map(AsRef::as_ref).collect();
        assert_eq!(
            kex,
            [
                "curve25519-sha256",
                "curve25519-sha256@libssh.org",
                "diffie-hellman-group16-sha512",
                "diffie-hellman-group14-sha256",
                "ext-info-c",
                "ext-info-s",
                "kex-strict-c-v00@openssh.com",
                "kex-strict-s-v00@openssh.com"
            ]
        );
        assert!(preferred.mac.iter().all(|m| !m.as_ref().contains("sha1")));
    }

    #[test]
    fn rsa_signing_hash_never_selects_sha1() {
        assert_eq!(rsa_signing_hash(None), Some(HashAlg::Sha256));
        assert_eq!(
            rsa_signing_hash(Some(HashAlg::Sha256)),
            Some(HashAlg::Sha256)
        );
        assert_eq!(
            rsa_signing_hash(Some(HashAlg::Sha512)),
            Some(HashAlg::Sha512)
        );
    }

    fn handler_in(dir: &tempfile::TempDir) -> ClientHandler {
        ClientHandler {
            endpoint: "host.example.com:22".to_string(),
            known_hosts: dir.path().join("ssh_known_hosts.json"),
        }
    }

    #[tokio::test]
    async fn check_server_key_records_first_key_and_accepts_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut handler = handler_in(&dir);
        let key = parse(ED25519_A);
        assert!(handler.check_server_key(&key).await.unwrap());
        assert!(handler.check_server_key(&key).await.unwrap());
        let stored: HashMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&handler.known_hosts).unwrap()).unwrap();
        assert_eq!(stored["host.example.com:22"], ED25519_A_ID);
    }

    #[tokio::test]
    async fn check_server_key_rejects_a_changed_host_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut handler = handler_in(&dir);
        assert!(handler.check_server_key(&parse(ED25519_A)).await.unwrap());
        assert!(!handler.check_server_key(&parse(ED25519_B)).await.unwrap());
        // The rejected key must not overwrite the trusted one.
        assert!(handler.check_server_key(&parse(ED25519_A)).await.unwrap());
    }
}
