//! russh-backed SSH session: connect (password/key), exec, and SFTP push/pull.
//! Server keys are verified trust-on-first-use against `~/.rantaiclaw/ssh_known_hosts.json`.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
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
/// Returns an error if the TCP/SSH handshake fails, the server key is refused
/// by the trust-on-first-use check, authentication is rejected, or key material
/// cannot be loaded.
pub async fn connect(host: &str, port: u16, user: &str, auth: Auth) -> Result<String> {
    let config = Arc::new(client_config());
    let handler = ClientHandler {
        endpoint: format!("{host}:{port}"),
        known_hosts: known_hosts_path(),
    };
    let mut handle = russh::client::connect(config, (host, port), handler)
        .await
        .map_err(|e| connect_error(host, port, e))?;

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

/// How many temporary names the atomic store write tries before it gives up.
const TEMP_FILE_ATTEMPTS: u32 = 8;

struct ClientHandler {
    endpoint: String,
    known_hosts: PathBuf,
}

/// What the trust-on-first-use check concluded about one server key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TofuOutcome {
    /// The presented key is the one already stored for this endpoint.
    Matched { key_id: String },
    /// The endpoint was unseen; the key was pinned and the store was written.
    NewlyPinned { key_id: String },
    /// The endpoint is pinned with a different key.
    KeyChanged {
        stored_id: String,
        presented_id: String,
    },
    /// The store exists but cannot be read.
    StoreUnreadable { path: PathBuf, error: String },
    /// The store exists but is not valid JSON.
    StoreUnparseable { path: PathBuf, error: String },
    /// The store cannot be written.
    StoreUnwritable { path: PathBuf, error: String },
}

/// Why a connection was refused before authentication.
#[derive(Debug)]
enum SshError {
    /// The server key was refused by the trust-on-first-use check.
    HostKeyRefused {
        endpoint: String,
        path: PathBuf,
        outcome: TofuOutcome,
    },
    /// Any other transport-level russh failure.
    Transport(russh::Error),
}

impl From<russh::Error> for SshError {
    fn from(e: russh::Error) -> Self {
        SshError::Transport(e)
    }
}

impl std::fmt::Display for SshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshError::HostKeyRefused {
                endpoint,
                path,
                outcome,
            } => f.write_str(&refusal_message(endpoint, path, outcome)),
            SshError::Transport(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SshError {}

impl Handler for ClientHandler {
    type Error = SshError;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let outcome = tofu_check(
            &self.known_hosts,
            &self.endpoint,
            &host_key_id(server_public_key),
        );
        match outcome {
            TofuOutcome::Matched { .. } | TofuOutcome::NewlyPinned { .. } => Ok(true),
            outcome => Err(SshError::HostKeyRefused {
                endpoint: self.endpoint.clone(),
                path: self.known_hosts.clone(),
                outcome,
            }),
        }
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

/// Trust-on-first-use: report what the stored key means for this endpoint.
/// Every path that cannot complete leaves the connection refused: a store that
/// cannot be read, cannot be parsed, or cannot be written stops the connection
/// instead of quietly behaving like an empty store.
fn tofu_check(path: &Path, endpoint: &str, key_id: &str) -> TofuOutcome {
    let mut map: HashMap<String, String> = match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(map) => map,
            Err(e) => {
                return TofuOutcome::StoreUnparseable {
                    path: path.to_path_buf(),
                    error: e.to_string(),
                };
            }
        },
        // No store yet: the normal first-contact case.
        Err(e) if store_is_absent(&e) => HashMap::new(),
        Err(e) => {
            return TofuOutcome::StoreUnreadable {
                path: path.to_path_buf(),
                error: e.to_string(),
            };
        }
    };

    match map.get(endpoint) {
        // The trusted path: no write, no permission change, no message.
        Some(stored) if stored == key_id => TofuOutcome::Matched {
            key_id: key_id.to_string(),
        },
        Some(stored) => TofuOutcome::KeyChanged {
            stored_id: stored.clone(),
            presented_id: key_id.to_string(),
        },
        None => {
            map.insert(endpoint.to_string(), key_id.to_string());
            match write_store(path, &map) {
                Ok(()) => TofuOutcome::NewlyPinned {
                    key_id: key_id.to_string(),
                },
                // The host stays unpinned and the connection is refused.
                Err(error) => TofuOutcome::StoreUnwritable {
                    path: path.to_path_buf(),
                    error,
                },
            }
        }
    }
}

/// True when the store is simply not there yet. `NotADirectory` means a parent
/// path component is a regular file, so no store can live there either; the
/// write reports that as a write failure.
fn store_is_absent(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Write the known-hosts store atomically: a temporary file in the same
/// directory, renamed over the target. A crash or a full disk leaves either
/// the old file or the new one, never a half-written store under the target
/// name. An existing store keeps its mode; a new one is created `0600`.
///
/// Returns the error text on failure.
fn write_store(path: &Path, map: &HashMap<String, String>) -> Result<(), String> {
    let serialized = serde_json::to_string_pretty(map).map_err(|e| e.to_string())?;

    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?;

    // An existing store keeps its mode. A read-only store is a write failure:
    // the operator marked that file read-only, and replacing it through the
    // directory would bypass that.
    let mut target_mode: Option<u32> = None;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create directory {}: {e}", parent.display()))?;
    match std::fs::metadata(path) {
        Ok(meta) => {
            if meta.is_dir() {
                return Err("the store path is a directory".to_string());
            }
            if meta.permissions().readonly() {
                return Err("the file is read-only".to_string());
            }
            target_mode = store_mode(&meta);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot stat the file: {e}")),
    }

    let (tmp_path, mut file) = create_temp_file(parent, file_name, target_mode)?;
    if let Err(e) = file.write_all(serialized.as_bytes()) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("cannot write {}: {e}", tmp_path.display()));
    }
    if let Err(e) = file.sync_all() {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("cannot flush {}: {e}", tmp_path.display()));
    }
    drop(file);
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("cannot replace {}: {e}", path.display()));
    }
    Ok(())
}

/// The permission bits an existing store carries, so a rewrite keeps them.
/// Unix is the only platform with POSIX mode bits to preserve.
#[cfg(unix)]
fn store_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

/// Unix is the only platform with POSIX mode bits to preserve.
#[cfg(not(unix))]
fn store_mode(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Create a uniquely named temporary file next to the store, carrying the mode
/// the store must end up with. `create_new` never touches an existing file.
fn create_temp_file(
    parent: &Path,
    file_name: &std::ffi::OsStr,
    target_mode: Option<u32>,
) -> Result<(PathBuf, std::fs::File), String> {
    let base = file_name.to_string_lossy();
    let mut last_error = String::new();
    for attempt in 0..TEMP_FILE_ATTEMPTS {
        let tmp_path = parent.join(format!("{base}.tmp.{}.{attempt}", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // The umask may still clear bits, so the exact mode is set below.
            options.mode(0o600);
        }
        match options.open(&tmp_path) {
            Ok(file) => {
                if let Err(e) = apply_mode(&tmp_path, target_mode) {
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(e);
                }
                return Ok((tmp_path, file));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = format!("{} already exists", tmp_path.display());
            }
            Err(e) => return Err(format!("cannot create {}: {e}", tmp_path.display())),
        }
    }
    Err(format!(
        "cannot create a temporary file next to {}: {last_error}",
        parent.display()
    ))
}

/// Set the exact permission bits on a file, defeating the process umask.
#[cfg(unix)]
fn apply_mode(path: &Path, mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = mode.unwrap_or(0o600);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("cannot set mode {mode:o} on {}: {e}", path.display()))
}

/// Unix is the only platform with POSIX mode bits to set.
#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: Option<u32>) -> Result<(), String> {
    Ok(())
}

/// The message a refused server key produces. It names the endpoint, the
/// store and the concrete next action. It never offers a way to skip the
/// check: there is none.
fn refusal_message(endpoint: &str, path: &Path, outcome: &TofuOutcome) -> String {
    let path = path.display();
    match outcome {
        TofuOutcome::KeyChanged {
            stored_id,
            presented_id,
        } => format!(
            "ssh server key for {endpoint} changed: stored {stored_id}, presented {presented_id}; \
             the known-hosts file {path} was left unchanged - if the change is expected, remove \
             the entry for {endpoint} from {path} deliberately before connecting again"
        ),
        TofuOutcome::StoreUnparseable { error, .. } => format!(
            "ssh known-hosts file {path} is not valid JSON ({error}); it was left untouched - \
             repair it or remove it before connecting again"
        ),
        TofuOutcome::StoreUnwritable { error, .. } => format!(
            "ssh known-hosts file {path} cannot be written ({error}); fix the file or the \
             directory that holds it, then connect again"
        ),
        TofuOutcome::StoreUnreadable { error, .. } => format!(
            "ssh known-hosts file {path} cannot be read ({error}); fix the file or the directory \
             that holds it, then connect again"
        ),
        // Unreachable. A matched or newly pinned key is accepted, so it never produces a
        // refusal. The arm keeps the match exhaustive.
        TofuOutcome::Matched { key_id } | TofuOutcome::NewlyPinned { key_id } => {
            format!("ssh server key for {endpoint} is {key_id} in {path}")
        }
    }
}

/// The message a failed handshake produces: a refused server key names the
/// endpoint and the store, every other failure keeps the transport message.
fn connect_error(host: &str, port: u16, error: SshError) -> anyhow::Error {
    match error {
        SshError::HostKeyRefused {
            endpoint,
            path,
            outcome,
        } => anyhow!("{}", refusal_message(&endpoint, &path, &outcome)),
        SshError::Transport(e) => anyhow!("ssh connect to {host}:{port} failed: {e}"),
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

        let error = handler
            .check_server_key(&parse(ED25519_B))
            .await
            .unwrap_err();
        match error {
            SshError::HostKeyRefused { outcome, .. } => assert_eq!(
                outcome,
                TofuOutcome::KeyChanged {
                    stored_id: ED25519_A_ID.to_string(),
                    presented_id: ED25519_B_ID.to_string(),
                }
            ),
            other @ SshError::Transport(_) => {
                panic!("expected a host-key refusal, got {other:?}")
            }
        }
        // The rejected key must not overwrite the trusted one.
        assert!(handler.check_server_key(&parse(ED25519_A)).await.unwrap());
        let stored: HashMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&handler.known_hosts).unwrap()).unwrap();
        assert_eq!(stored["host.example.com:22"], ED25519_A_ID);
    }

    #[test]
    fn tofu_check_refuses_an_unparseable_store_and_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_known_hosts.json");
        std::fs::write(&path, "{ not json").unwrap();

        let outcome = tofu_check(&path, "host.example.com:22", ED25519_A_ID);

        match outcome {
            TofuOutcome::StoreUnparseable { path: named, .. } => assert_eq!(named, path),
            other => panic!("expected an unparseable store, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn tofu_check_refuses_an_unreadable_store_and_creates_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_known_hosts.json");
        std::fs::create_dir(&path).unwrap();

        let outcome = tofu_check(&path, "host.example.com:22", ED25519_A_ID);

        match outcome {
            TofuOutcome::StoreUnreadable { path: named, .. } => assert_eq!(named, path),
            other => panic!("expected an unreadable store, got {other:?}"),
        }
        assert!(
            path.is_dir(),
            "the store path must still be the directory it was"
        );
    }

    #[test]
    fn tofu_check_refuses_a_store_that_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        // A regular file where a directory is needed: creating the store's
        // parent directory fails, as root too.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let path = blocker.join("child").join("ssh_known_hosts.json");

        let outcome = tofu_check(&path, "host.example.com:22", ED25519_A_ID);

        match outcome {
            TofuOutcome::StoreUnwritable { path: named, .. } => assert_eq!(named, path),
            other => panic!("expected an unwritable store, got {other:?}"),
        }
        assert!(!path.exists(), "no store may be created");
    }

    #[cfg(unix)]
    #[test]
    fn tofu_check_refuses_a_read_only_store() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_known_hosts.json");
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let outcome = tofu_check(&path, "host.example.com:22", ED25519_A_ID);

        match outcome {
            TofuOutcome::StoreUnwritable { path: named, error } => {
                assert_eq!(named, path);
                assert!(
                    error.contains("read-only"),
                    "the error must say the file is read-only: {error}"
                );
            }
            other => panic!("expected an unwritable store, got {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");
    }

    #[test]
    fn tofu_check_reports_a_changed_key_with_both_fingerprints_and_keeps_the_stored_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_known_hosts.json");
        assert_eq!(
            tofu_check(&path, "host.example.com:22", ED25519_A_ID),
            TofuOutcome::NewlyPinned {
                key_id: ED25519_A_ID.to_string()
            }
        );

        let outcome = tofu_check(&path, "host.example.com:22", ED25519_B_ID);

        assert_eq!(
            outcome,
            TofuOutcome::KeyChanged {
                stored_id: ED25519_A_ID.to_string(),
                presented_id: ED25519_B_ID.to_string(),
            }
        );
        let stored: HashMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["host.example.com:22"], ED25519_A_ID);
    }

    #[test]
    fn tofu_check_pins_a_new_host_and_writes_the_store_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_known_hosts.json");

        let outcome = tofu_check(&path, "host.example.com:22", ED25519_A_ID);

        assert_eq!(
            outcome,
            TofuOutcome::NewlyPinned {
                key_id: ED25519_A_ID.to_string()
            }
        );
        assert!(path.exists());
        let stored: HashMap<String, String> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["host.example.com:22"], ED25519_A_ID);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            assert_eq!(mode, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn tofu_check_keeps_the_mode_of_an_existing_store() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_known_hosts.json");
        std::fs::write(&path, "{}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let outcome = tofu_check(&path, "host.example.com:22", ED25519_A_ID);

        assert_eq!(
            outcome,
            TofuOutcome::NewlyPinned {
                key_id: ED25519_A_ID.to_string()
            }
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn a_matching_key_is_accepted_with_a_read_only_store() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_known_hosts.json");
        assert_eq!(
            tofu_check(&path, "host.example.com:22", ED25519_A_ID),
            TofuOutcome::NewlyPinned {
                key_id: ED25519_A_ID.to_string()
            }
        );
        let before = std::fs::read_to_string(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = tofu_check(&path, "host.example.com:22", ED25519_A_ID);

        assert_eq!(
            outcome,
            TofuOutcome::Matched {
                key_id: ED25519_A_ID.to_string()
            }
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // Give the temp dir write access back so it can be removed.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn the_store_write_leaves_no_temporary_file_and_replaces_the_target() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh_known_hosts.json");
        assert_eq!(
            tofu_check(&path, "host.example.com:22", ED25519_A_ID),
            TofuOutcome::NewlyPinned {
                key_id: ED25519_A_ID.to_string()
            }
        );
        let first_inode = std::fs::metadata(&path).unwrap().ino();

        assert_eq!(
            tofu_check(&path, "other.example.com:22", ED25519_B_ID),
            TofuOutcome::NewlyPinned {
                key_id: ED25519_B_ID.to_string()
            }
        );

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["ssh_known_hosts.json".to_string()]);
        let second_inode = std::fs::metadata(&path).unwrap().ino();
        assert_ne!(
            first_inode, second_inode,
            "the store must be reached by a rename, not written in place"
        );
    }

    #[tokio::test]
    async fn check_server_key_refuses_a_changed_host_key_with_both_fingerprints() {
        let dir = tempfile::tempdir().unwrap();
        let mut handler = handler_in(&dir);
        assert!(handler.check_server_key(&parse(ED25519_A)).await.unwrap());

        let error = handler
            .check_server_key(&parse(ED25519_B))
            .await
            .unwrap_err();
        let message = error.to_string();
        match error {
            SshError::HostKeyRefused {
                endpoint,
                path,
                outcome,
            } => {
                assert_eq!(endpoint, "host.example.com:22");
                assert_eq!(path, handler.known_hosts);
                assert_eq!(
                    outcome,
                    TofuOutcome::KeyChanged {
                        stored_id: ED25519_A_ID.to_string(),
                        presented_id: ED25519_B_ID.to_string(),
                    }
                );
            }
            other @ SshError::Transport(_) => {
                panic!("expected a host-key refusal, got {other:?}")
            }
        }
        assert!(
            message.contains(ED25519_A_ID),
            "the message must name the stored key: {message}"
        );
        assert!(
            message.contains(ED25519_B_ID),
            "the message must name the presented key: {message}"
        );
        assert!(
            message.contains(&handler.known_hosts.display().to_string()),
            "the message must name the store: {message}"
        );
    }

    #[tokio::test]
    async fn a_refused_host_key_is_refused_and_leaves_no_session_for_that_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut handler = ClientHandler {
            endpoint: "audit@127.0.0.1:22022".to_string(),
            known_hosts: dir.path().join("ssh_known_hosts.json"),
        };
        assert!(handler.check_server_key(&parse(ED25519_A)).await.unwrap());

        let error = handler
            .check_server_key(&parse(ED25519_B))
            .await
            .unwrap_err();

        assert!(
            matches!(error, SshError::HostKeyRefused { .. }),
            "expected a host-key refusal, got {error:?}"
        );
        // The registry is written by `connect` only after the handshake and the authentication
        // succeed, so a refused key can never leave a session behind; this pins that invariant
        // locally. The end-to-end proof is the `192-5` drive case: the `exec` after the refused
        // `connect` answers "no ssh session `audit@127.0.0.1:22022` (connect first)".
        assert!(crate::remote::registry::get("audit@127.0.0.1:22022")
            .await
            .is_none());
    }
}
