//! `rantaiclaw ui` — install and run the optional web console (claw-ui).
//!
//! The console is a separate Next.js app
//! (<https://github.com/RantAI-dev/claw-ui>). It is intentionally NOT bundled in
//! the binary — this command fetches a signed, prebuilt release archive on
//! demand into `~/.rantaiclaw/ui` and serves it (`node server.js`) against a
//! local gateway. No `git` clone and no on-machine JS build; only `tar` (to
//! extract) and `node` (to run the standalone server) are required.

use std::collections::BTreeMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rantaiclaw::lifecycle::artifact::{self, CosignOutcome};

const DEFAULT_PORT: u16 = 3939;

/// Pinned claw-ui release tag `ui install` fetches by default. Bump this to
/// roll the console forward; overridable per-invocation with `--ref`.
const CLAW_UI_RELEASE: &str = "v0.3.0";
const CLAW_UI_REPO: &str = "https://github.com/RantAI-dev/claw-ui";
/// Expected cosign signing identity for claw-ui releases — its `release.yml`
/// workflow on a tag ref. Passed to the shared `lifecycle::artifact::verify_cosign`.
const CLAW_UI_COSIGN_IDENTITY: &str =
    r"^https://github\.com/RantAI-dev/claw-ui/\.github/workflows/release\.yml@.*$";

/// Reject anything that isn't a plain tag/branch token so `--ref` cannot
/// retarget the download URL to another repo or path.
fn validate_ref(r: &str) -> Result<()> {
    let ok = !r.is_empty()
        && r.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !ok {
        bail!("invalid ref '{r}': only letters, digits, '.', '_', '-' are allowed");
    }
    Ok(())
}

/// `(archive_url, sums_url, archive_name)` for a claw-ui release tag.
fn claw_ui_urls(tag: &str) -> (String, String, String) {
    let base = format!("{CLAW_UI_REPO}/releases/download/{tag}");
    let name = format!("claw-ui-{tag}.tar.gz");
    (format!("{base}/{name}"), format!("{base}/SHA256SUMS"), name)
}

/// Fresh scratch dir for downloading + verifying a release archive before
/// extraction. `tempfile` is a dev-dependency only, so this mirrors
/// `lifecycle::update::make_work_dir` rather than using `tempfile::tempdir()`.
fn make_ui_work_dir() -> Result<PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("rantaiclaw-ui-{pid}-{nanos:x}"));
    std::fs::create_dir_all(&dir).with_context(|| format!("create temp dir {}", dir.display()))?;
    Ok(dir)
}

/// Default install location: `~/.rantaiclaw/ui` (shared across profiles).
fn default_dir() -> PathBuf {
    crate::profile::paths::rantaiclaw_root().join("ui")
}

/// True when `bin --version` runs successfully — a cheap availability probe.
fn has_binary(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Connect address for a bind host (`0.0.0.0`/empty → loopback).
fn connect_host(host: &str) -> &str {
    if host.is_empty() || host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    }
}

/// True when something accepts TCP connections at `host:port`.
fn port_open(host: &str, port: u16) -> bool {
    format!("{}:{}", connect_host(host), port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
        .map(|sa| TcpStream::connect_timeout(&sa, Duration::from_millis(700)).is_ok())
        .unwrap_or(false)
}

/// Poll `host:port` until it accepts connections or `secs` elapse.
fn wait_for_port(host: &str, port: u16, secs: u64) -> bool {
    for _ in 0..(secs * 2) {
        if port_open(host, port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    port_open(host, port)
}

/// Split an `http://host:port` URL into (host, port), defaulting the port to 80.
fn split_host_port(url: &str) -> (String, u16) {
    let s = url
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let s = s.split('/').next().unwrap_or(s);
    match s.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(80)),
        None => (s.to_string(), 80),
    }
}

/// Spawn a detached background process (own session, stdio → `log`), returning its PID.
fn spawn_detached(cmd: &mut Command, log: &Path) -> Result<u32> {
    // Truncate (don't append) so each launch starts with a clean gateway log.
    let out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(log)
        .with_context(|| format!("open {}", log.display()))?;
    let err = out.try_clone()?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // On Unix, setsid() in the child detaches it from our session/terminal so it keeps running
    // after `ui start` returns and survives the terminal closing. No-op on other platforms.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid() only starts a new session in the post-fork child; nothing else runs there.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let child = cmd.spawn().with_context(|| format!("spawn {cmd:?}"))?;
    Ok(child.id())
}

/// Identity reported by a gateway's `GET /api/v1/version`.
#[derive(Debug, Clone, PartialEq)]
struct GatewayIdentity {
    name: String,
    version: String,
    config_fingerprint: String,
}

/// What `ui start` should do about whatever is (or isn't) on the gateway port.
#[derive(Debug, Clone, Copy, PartialEq)]
enum GatewayAction {
    StartFresh,       // nothing on the port
    Reuse,            // rantaiclaw gateway, current version + fingerprint
    Restart,          // rantaiclaw gateway, but version/config drift
    ForeignError,     // something answered, but not a rantaiclaw gateway
    UnconfirmedError, // port busy, no valid /api/v1/version reply
}

/// Decide what to do about the gateway port given whether it's occupied and,
/// if so, what identified itself there. Pure so it's cheap to test exhaustively.
fn decide_gateway_action(
    present: bool,
    ident: Option<GatewayIdentity>,
    current_version: &str,
    disk_fp: &str,
) -> GatewayAction {
    if !present {
        return GatewayAction::StartFresh;
    }
    match ident {
        None => GatewayAction::UnconfirmedError,
        Some(id) if id.name != "rantaiclaw" => GatewayAction::ForeignError,
        Some(id) if id.version == current_version && id.config_fingerprint == disk_fp => {
            GatewayAction::Reuse
        }
        Some(_) => GatewayAction::Restart,
    }
}

#[cfg(test)]
mod gateway_action_tests {
    use super::*;

    fn ident(v: &str, fp: &str) -> Option<GatewayIdentity> {
        Some(GatewayIdentity {
            name: "rantaiclaw".into(),
            version: v.into(),
            config_fingerprint: fp.into(),
        })
    }

    #[test]
    fn nothing_on_port_starts_fresh() {
        assert_eq!(
            decide_gateway_action(false, None, "1.0", "aa"),
            GatewayAction::StartFresh
        );
    }
    #[test]
    fn fresh_gateway_is_reused() {
        assert_eq!(
            decide_gateway_action(true, ident("1.0", "aa"), "1.0", "aa"),
            GatewayAction::Reuse
        );
    }
    #[test]
    fn stale_config_triggers_restart() {
        assert_eq!(
            decide_gateway_action(true, ident("1.0", "OLD"), "1.0", "NEW"),
            GatewayAction::Restart
        );
    }
    #[test]
    fn stale_version_triggers_restart() {
        assert_eq!(
            decide_gateway_action(true, ident("0.9", "aa"), "1.0", "aa"),
            GatewayAction::Restart
        );
    }
    #[test]
    fn foreign_app_errors() {
        let other = Some(GatewayIdentity {
            name: "vite".into(),
            version: "x".into(),
            config_fingerprint: "y".into(),
        });
        assert_eq!(
            decide_gateway_action(true, other, "1.0", "aa"),
            GatewayAction::ForeignError
        );
    }
    #[test]
    fn busy_but_silent_is_unconfirmed() {
        assert_eq!(
            decide_gateway_action(true, None, "1.0", "aa"),
            GatewayAction::UnconfirmedError
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ref_accepts_a_tag_and_rejects_traversal() {
        assert!(validate_ref("v0.3.0").is_ok());
        assert!(validate_ref("main").is_ok());
        assert!(validate_ref("../../other-repo/x").is_err());
        assert!(validate_ref("a b").is_err());
        assert!(validate_ref("").is_err());
    }

    #[test]
    fn ssh_forward_hint_builds_command_in_ssh_session() {
        let hint = ssh_forward_hint(Some("192.168.18.104 64231 192.168.18.170 22"), "bob", 3939)
            .expect("SSH session should produce a hint");
        assert!(hint.contains("ssh -L 3939:127.0.0.1:3939 bob@192.168.18.170"));
        assert!(hint.contains("http://127.0.0.1:3939"));
    }

    #[test]
    fn ssh_forward_hint_absent_without_ssh_or_when_malformed() {
        assert!(ssh_forward_hint(None, "bob", 3939).is_none());
        assert!(ssh_forward_hint(Some("a b"), "bob", 3939).is_none());
    }

    #[test]
    fn claw_ui_urls_builds_release_asset_paths() {
        let (archive_url, sums_url, name) = claw_ui_urls("v0.3.0");
        assert_eq!(name, "claw-ui-v0.3.0.tar.gz");
        assert_eq!(
            archive_url,
            "https://github.com/RantAI-dev/claw-ui/releases/download/v0.3.0/claw-ui-v0.3.0.tar.gz"
        );
        assert_eq!(
            sums_url,
            "https://github.com/RantAI-dev/claw-ui/releases/download/v0.3.0/SHA256SUMS"
        );
    }

    #[test]
    fn managed_dir_is_detected_by_server_js() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("server.js"), "// standalone").unwrap();
        assert!(dir.join("server.js").exists());
    }

    #[test]
    fn console_env_pins_loopback_and_passes_secrets() {
        let env = console_env(3939, "http://127.0.0.1:4939", "tok", "sec");
        assert_eq!(env.get("HOSTNAME").map(String::as_str), Some("127.0.0.1"));
        assert_eq!(env.get("PORT").map(String::as_str), Some("3939"));
        assert_eq!(
            env.get("RANTAICLAW_GATEWAY_URL").map(String::as_str),
            Some("http://127.0.0.1:4939")
        );
        assert_eq!(env.get("RANTAICLAW_TOKEN").map(String::as_str), Some("tok"));
        assert_eq!(
            env.get("RANTAICLAW_UI_SECRET").map(String::as_str),
            Some("sec")
        );
    }
}

/// GET http://host:port/api/v1/version via curl, parse the identity. Retries
/// once (the gateway may be momentarily busy). `None` on repeated failure.
fn probe_gateway_identity(gw_host: &str, gw_port: u16) -> Option<GatewayIdentity> {
    let url = format!("http://{gw_host}:{gw_port}/api/v1/version");
    for attempt in 0..2 {
        if attempt == 1 {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        let out = Command::new("curl")
            .args(["-fsS", "--max-time", "3", &url])
            .output();
        if let Ok(out) = out {
            if out.status.success() {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                    if let (Some(name), Some(version)) = (
                        v.get("name").and_then(|x| x.as_str()),
                        v.get("version").and_then(|x| x.as_str()),
                    ) {
                        return Some(GatewayIdentity {
                            name: name.to_string(),
                            version: version.to_string(),
                            config_fingerprint: v
                                .get("config_fingerprint")
                                .and_then(|f| f.as_str())
                                .unwrap_or("none")
                                .to_string(),
                        });
                    }
                }
            }
        }
    }
    None
}

/// Stop a stale RantaiClaw gateway on `gw_port`. Prefer the PID we recorded when
/// we started it (`<dir>/.run` `gateway=<pid>`), else discover the listener PID.
/// BOTH paths confirm the PID is a rantaiclaw gateway before signalling (guards
/// against PID reuse). Err with a manual instruction if it cannot be safely stopped.
fn stop_stale_gateway(gw_port: u16, dir: &Path) -> Result<()> {
    // `webui` is a binary-crate module and `lifecycle` is only compiled into the
    // lib crate (unlike `config`, which the binary re-declares), so reach it via
    // the `rantaiclaw` lib crate rather than `crate::`.
    use rantaiclaw::lifecycle::process as proc;

    // 1. PID recorded in <dir>/.run (`gateway=<pid>`), if ui start launched it.
    if let Ok(state) = std::fs::read_to_string(run_file(dir)) {
        if let Some(pid) = state.lines().find_map(|l| {
            l.strip_prefix("gateway=")
                .and_then(|s| s.trim().parse::<u32>().ok())
        }) {
            if proc::process_is_alive(pid)
                && proc::cmdline_contains(pid, &["rantaiclaw", "gateway"])
                && proc::stop_process_graceful(pid)
            {
                return Ok(());
            }
        }
    }

    // 2. Discover the listener PID; confirm identity before signalling.
    if let Some(pid) = proc::pid_listening_on_port(gw_port) {
        if proc::cmdline_contains(pid, &["rantaiclaw", "gateway"])
            && proc::stop_process_graceful(pid)
        {
            return Ok(());
        }
    }

    bail!(
        "a stale RantaiClaw gateway is on port {gw_port} but could not be stopped \
         automatically. Stop it manually and re-run `rantaiclaw ui start`."
    )
}

/// Start the gateway as a background process and wait for it to come up.
fn start_fresh_gateway(
    gw_host: &str,
    gw_port: u16,
    gateway: &str,
    gw_log: &Path,
    gateway_pid: &mut Option<u32>,
) -> Result<()> {
    println!("▶ starting gateway ({gateway}) …");
    let exe = std::env::current_exe().context("could not resolve the rantaiclaw binary path")?;
    let mut g = Command::new(&exe);
    g.arg("gateway")
        .arg("-p")
        .arg(gw_port.to_string())
        .arg("--host")
        .arg(gw_host);
    *gateway_pid = Some(spawn_detached(&mut g, gw_log)?);
    if !wait_for_port(gw_host, gw_port, 20) {
        bail!(
            "gateway did not come up on {gw_host}:{gw_port} — see {}",
            gw_log.display()
        );
    }
    Ok(())
}

/// Obtain a bearer token by minting a short-lived, single-use on-demand
/// "gateway" pairing code and exchanging it via `POST /pair`.
///
/// Unlike reading the one-time startup code from the gateway log, this works
/// against a gateway we did **not** start (a running daemon, whose startup code
/// we never see) and even once the gateway already holds tokens — the store
/// code is independent of the startup code, so a fresh install or a lost
/// `.env.local` can still pair without restarting the daemon. Returns `None`
/// when the profile can't be resolved, the store write fails, or `/pair`
/// rejects the code.
fn mint_and_pair(gateway_url: &str) -> Option<String> {
    let root = crate::profile::ProfileManager::active().ok()?.root;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let code =
        crate::security::pairing_store::mint(&root, "gateway", 120, Some(1), false, now).ok()?;
    pair_token(gateway_url, &code)
}

/// Exchange a pairing code for a bearer token via `POST /pair` (uses `curl`).
fn pair_token(gateway_url: &str, code: &str) -> Option<String> {
    let out = Command::new("curl")
        .args([
            "-fsS",
            "-X",
            "POST",
            &format!("{gateway_url}/pair"),
            "-H",
            &format!("X-Pairing-Code: {code}"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let body = String::from_utf8_lossy(&out.stdout);
    let key = "\"token\":\"";
    let start = body.find(key)? + key.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// True ONLY when the gateway explicitly rejects `token` (HTTP 401/403) on an
/// authenticated endpoint. A transport error, missing `curl`, timeout, or any
/// other status returns false — a possibly-valid token must never be discarded
/// on a transient failure (fail-safe: worst case we keep today's behavior).
fn token_rejected(gateway_url: &str, token: &str) -> bool {
    let out = Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-m",
            "5",
            &format!("{gateway_url}/api/v1/config"),
            "-H",
            &format!("Authorization: Bearer {token}"),
        ])
        .output();
    match out {
        // curl exits 0 even on 401 (no -f) — nonzero means transport failure.
        Ok(o) if o.status.success() => {
            matches!(String::from_utf8_lossy(&o.stdout).trim(), "401" | "403")
        }
        _ => false,
    }
}

/// Path of the run-state file recording the background gateway/UI PIDs.
fn run_file(dir: &Path) -> PathBuf {
    dir.join(".run")
}

pub fn handle_command(command: &crate::UiCommands, config: &crate::config::Config) -> Result<()> {
    match command {
        crate::UiCommands::Install { dir, r#ref, force } => {
            install(dir.clone(), r#ref.clone(), *force)
        }
        crate::UiCommands::Start {
            dir,
            port,
            gateway,
            token,
        } => start(dir.clone(), *port, gateway.clone(), token.clone(), config),
        crate::UiCommands::Stop { dir } => stop(dir.clone()),
        crate::UiCommands::Path { dir } => {
            println!("{}", dir.clone().unwrap_or_else(default_dir).display());
            Ok(())
        }
    }
}

/// Download, verify (SHA256 + cosign), and extract a signed claw-ui release
/// archive into `dir`. No `git` clone, no on-machine JS build.
fn install(dir: Option<PathBuf>, git_ref: Option<String>, force: bool) -> Result<()> {
    let dir = dir.unwrap_or_else(default_dir);
    let tag = git_ref.as_deref().unwrap_or(CLAW_UI_RELEASE);
    validate_ref(tag)?;

    if !has_binary("tar") {
        bail!("`tar` is required to install the web console — install tar and retry");
    }

    // Refuse to clobber a non-empty dir that we did not create, unless --force.
    // `.git` covers a directory left over from the previous git-clone-based installer.
    let managed = dir.join("server.js").exists() || dir.join(".git").is_dir();
    let non_empty = dir
        .read_dir()
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if non_empty && !managed && !force {
        bail!(
            "{} exists and is not empty — pass --force to overwrite",
            dir.display()
        );
    }

    let (archive_url, sums_url, archive_name) = claw_ui_urls(tag);
    // Release download base — the dir cosign appends `<archive>.bundle` to.
    let base_url = format!("{CLAW_UI_REPO}/releases/download/{tag}");

    // `tempfile` is a dev-dependency only, so we manage + clean up our own
    // scratch dir rather than `tempfile::tempdir()`. Run the fallible fetch +
    // verify + extract steps in a closure so cleanup runs on every exit path.
    let work = make_ui_work_dir()?;
    let result = (|| -> Result<()> {
        let archive_path = work.join(&archive_name);
        let sums_path = work.join("SHA256SUMS");

        println!("⤓ Downloading {archive_name} ({tag}) …");
        artifact::download_to(&archive_url, &archive_path)?;
        artifact::download_to(&sums_url, &sums_path)?;

        artifact::verify_sha256(&archive_path, &sums_path, &archive_name)?;
        println!("✓ SHA256 verified");

        // Fail closed on a missing bundle: claw-ui is signed from its first release,
        // so an absent bundle means tampering (signature-stripping downgrade), not
        // a legacy pre-cosign tag. Only "cosign not installed locally" degrades.
        match artifact::verify_cosign(
            &base_url,
            &archive_path,
            &archive_name,
            &work,
            CLAW_UI_COSIGN_IDENTITY,
        )? {
            CosignOutcome::Verified => println!("✓ cosign signature verified"),
            CosignOutcome::CosignNotInstalled => {} // helper already warned
            CosignOutcome::BundleMissing => bail!(
                "no cosign signature published for {tag} — refusing to install an \
                 unsigned console artifact (possible tampering)"
            ),
        }

        // Verified: safe to extract. Wipe any prior layout (managed dir).
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("clear {}", dir.display()))?;
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let status = Command::new("tar")
            .arg("xzf")
            .arg(&archive_path)
            .arg("-C")
            .arg(&dir)
            .status()
            .context("extract console archive")?;
        if !status.success() {
            bail!("tar extraction failed ({status})");
        }
        if !dir.join("server.js").exists() {
            bail!("extracted archive has no server.js — unexpected artifact layout");
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result?;

    println!("\n✅ Web console installed at {}", dir.display());
    println!("   Start it with:  rantaiclaw ui start");
    Ok(())
}

/// Environment for the standalone console process. `HOSTNAME=127.0.0.1` pins
/// the Next.js standalone server to loopback — it binds `0.0.0.0` otherwise,
/// which would be an exposure-boundary regression.
fn console_env(port: u16, gateway: &str, token: &str, ui_secret: &str) -> BTreeMap<String, String> {
    let mut e = BTreeMap::new();
    e.insert("PORT".into(), port.to_string());
    e.insert("HOSTNAME".into(), "127.0.0.1".into());
    e.insert("RANTAICLAW_GATEWAY_URL".into(), gateway.to_string());
    e.insert("RANTAICLAW_TOKEN".into(), token.to_string());
    e.insert("RANTAICLAW_UI_SECRET".into(), ui_secret.to_string());
    e
}

/// Bring up the console: ensure the gateway is running (auto-start + auto-pair if needed) and
/// launch the standalone `node server.js` console process — both in the background. Tear down
/// with `rantaiclaw ui stop`.
fn start(
    dir: Option<PathBuf>,
    port: Option<u16>,
    gateway: Option<String>,
    token: Option<String>,
    config: &crate::config::Config,
) -> Result<()> {
    let dir = dir.unwrap_or_else(default_dir);
    if !dir.join("server.js").exists() {
        bail!(
            "web console not installed at {} — run `rantaiclaw ui install` first",
            dir.display()
        );
    }
    if !has_binary("node") {
        bail!(
            "`node` is required to run the web console (Node >= 18.18) — install Node.js and retry"
        );
    }
    let port = port.unwrap_or(DEFAULT_PORT);

    // Resolve gateway + token without clobbering what's already there: an explicit flag wins,
    // then the environment, then whatever is already in `.env.local` (so a paired token survives
    // repeated `ui start`s — the previous behaviour blanked the token every run), then the config.
    let env_path = dir.join(".env.local");
    let existing = std::fs::read_to_string(&env_path).unwrap_or_default();
    let existing_val = |key: &str| -> Option<String> {
        existing.lines().find_map(|line| {
            line.trim()
                .strip_prefix(key)?
                .strip_prefix('=')
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
    };
    let nonempty = |s: String| if s.is_empty() { None } else { Some(s) };

    let gateway = gateway
        .and_then(nonempty)
        .or_else(|| {
            std::env::var("RANTAICLAW_GATEWAY_URL")
                .ok()
                .and_then(nonempty)
        })
        .or_else(|| existing_val("RANTAICLAW_GATEWAY_URL"))
        .unwrap_or_else(|| format!("http://{}:{}", config.gateway.host, config.gateway.port));

    let mut token = token
        .and_then(nonempty)
        .or_else(|| std::env::var("RANTAICLAW_TOKEN").ok().and_then(nonempty))
        .or_else(|| existing_val("RANTAICLAW_TOKEN"))
        .unwrap_or_default();

    // Cookie-signing secret for the console's session gate. Preserve an
    // operator-set value; otherwise generate a real random one. This is critical:
    // when console login is enabled, an empty/known secret would let anyone forge
    // an `rc_session` cookie and bypass the gate. We never leave it to claw-ui's
    // insecure built-in default. (The previous `.env.local` write also clobbered
    // an existing secret — preserving it here fixes that too.)
    let ui_secret = existing_val("RANTAICLAW_UI_SECRET")
        .or_else(|| {
            std::env::var("RANTAICLAW_UI_SECRET")
                .ok()
                .and_then(nonempty)
        })
        .unwrap_or_else(|| hex::encode(rand::random::<[u8; 32]>()));

    // Already serving on this port? Don't double-start.
    if port_open("127.0.0.1", port) {
        println!("✓ Web console already running → http://127.0.0.1:{port}");
        print_ssh_hint(port);
        println!("  Stop it with: rantaiclaw ui stop");
        return Ok(());
    }

    // 1. Ensure the gateway the console talks to is actually up.
    let (gw_host, gw_port) = split_host_port(&gateway);
    let gw_log = dir.join("gateway.log");
    let mut gateway_pid: Option<u32> = None;
    let gw_present = port_open(&gw_host, gw_port);
    let disk_fp = crate::config::fingerprint::fingerprint_file(&config.config_path);
    let action = decide_gateway_action(
        gw_present,
        if gw_present {
            probe_gateway_identity(&gw_host, gw_port)
        } else {
            None
        },
        env!("CARGO_PKG_VERSION"),
        &disk_fp,
    );
    match action {
        GatewayAction::Reuse => {
            println!("✓ gateway already running ({gateway})");
        }
        GatewayAction::ForeignError => {
            bail!(
                "port {gw_port} is in use by another application.\n  \
                 Set a different gateway port with `--port <n>` or `gateway.port` in \
                 config, then re-run `rantaiclaw ui start`."
            );
        }
        GatewayAction::UnconfirmedError => {
            // Re-check: the gateway may have exited since the port probe, in which
            // case a fresh start is correct rather than an error.
            if port_open(&gw_host, gw_port) {
                bail!(
                    "something is listening on port {gw_port} but it does not answer as a \
                     RantaiClaw gateway.\n  Check what is running there, or set a different \
                     gateway port with `--port <n>` / `gateway.port`, then re-run."
                );
            }
            start_fresh_gateway(&gw_host, gw_port, &gateway, &gw_log, &mut gateway_pid)?;
        }
        GatewayAction::Restart => {
            println!("↻ gateway on :{gw_port} is stale (version/config drift) — restarting…");
            stop_stale_gateway(gw_port, &dir)?;
            for _ in 0..20 {
                if !port_open(&gw_host, gw_port) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if port_open(&gw_host, gw_port) {
                bail!(
                    "port {gw_port} did not free after stopping the stale gateway; re-run shortly."
                );
            }
            start_fresh_gateway(&gw_host, gw_port, &gateway, &gw_log, &mut gateway_pid)?;
        }
        GatewayAction::StartFresh => {
            start_fresh_gateway(&gw_host, gw_port, &gateway, &gw_log, &mut gateway_pid)?;
        }
    }

    // 2. Self-heal a stale token. A token remembered in `.env.local` may have
    //    been issued by a *previous* gateway instance (an update/restart that
    //    reset `paired_tokens`, or another profile's gateway on the same port);
    //    reusing it blindly launches the console into 401s ("Gateway Offline").
    //    Probe an authed endpoint and drop the token ONLY on an explicit
    //    401/403 — transient probe failures keep it, so a flaky check can
    //    never make things worse than today's behavior.
    if !token.is_empty() && config.gateway.require_pairing && token_rejected(&gateway, &token) {
        println!("  ⚠ stored gateway token was rejected — re-pairing…");
        token.clear();
    }

    // 3. Pair if we don't (or no longer) have a token — regardless of who
    //    started the gateway. Auto-pair used to run only when *we* spawned the
    //    gateway (its one-time code was in our log); against an already-running
    //    daemon it was skipped, so the console launched token-less and the
    //    gateway answered 401 "requires pairing". Minting an on-demand
    //    "gateway" code and exchanging it via POST /pair works in both cases —
    //    and even once the daemon already holds tokens (a lost `.env.local`) —
    //    so no restart is ever needed. `require_pairing` stays authoritative.
    if token.is_empty() && config.gateway.require_pairing {
        match mint_and_pair(&gateway) {
            Some(t) => {
                token = t;
                println!("✓ paired with the gateway");
            }
            None => {
                println!(
                    "  ⚠ couldn't auto-pair with the gateway — the console will prompt for a \
                     pairing code (gateway log: {})",
                    gw_log.display()
                );
            }
        }
    }

    // 4. Launch the standalone console server. Gateway URL/token/secret are passed as
    //    process env only — never written to disk — and `HOSTNAME=127.0.0.1` pins the
    //    standalone server to loopback (it binds `0.0.0.0` by default otherwise).
    println!("▶ starting web console …");
    let ui_log = dir.join("ui.log");
    let mut c = Command::new("node");
    c.arg("server.js").current_dir(&dir);
    for (k, v) in console_env(port, &gateway, &token, &ui_secret) {
        c.env(k, v);
    }
    let ui_pid = spawn_detached(&mut c, &ui_log)?;

    // Record PIDs so `ui stop` can tear everything down.
    let mut state = String::new();
    if let Some(g) = gateway_pid {
        state.push_str(&format!("gateway={g}\n"));
    }
    state.push_str(&format!("ui={ui_pid}\n"));
    std::fs::write(run_file(&dir), state).ok();

    let ready = wait_for_port("127.0.0.1", port, 60);
    println!();
    if ready {
        println!("✓ Web console → http://127.0.0.1:{port}   (gateway: {gateway})");
    } else {
        println!("▶ Web console starting (first build can take a bit) → http://127.0.0.1:{port}");
    }
    print_ssh_hint(port);
    println!("  logs: {} · {}", ui_log.display(), gw_log.display());
    println!("  stop: rantaiclaw ui stop");
    Ok(())
}

/// When running over SSH, the loopback-only console isn't reachable from the
/// operator's own machine directly. Build a ready-to-copy `ssh -L` port-forward
/// command from `SSH_CONNECTION` (`<client_ip> <client_port> <server_ip>
/// <server_port>`) — field 3 is the exact address the operator connected to.
/// Returns `None` when not in an SSH session or the server IP can't be parsed.
fn ssh_forward_hint(ssh_connection: Option<&str>, user: &str, port: u16) -> Option<String> {
    let server_ip = ssh_connection?.split_whitespace().nth(2)?;
    if server_ip.is_empty() {
        return None;
    }
    Some(format!(
        "ℹ Remote session — reach it from your local machine:\n    \
         ssh -L {port}:127.0.0.1:{port} {user}@{server_ip}\n  \
         then open http://127.0.0.1:{port}"
    ))
}

/// Print the SSH port-forward hint when running in a remote session.
fn print_ssh_hint(port: u16) {
    let ssh_conn = std::env::var("SSH_CONNECTION").ok();
    let user = std::env::var("USER").unwrap_or_else(|_| "<user>".into());
    if let Some(hint) = ssh_forward_hint(ssh_conn.as_deref(), &user, port) {
        println!("{hint}");
    }
}

/// Terminate a background process (and its children) started by `ui start`.
#[cfg(unix)]
fn kill_process_group(pid: i32) -> bool {
    // Negative pid → the whole process group (the dev server spawns child node processes).
    unsafe { libc::kill(-pid, libc::SIGTERM) == 0 }
}

#[cfg(not(unix))]
fn kill_process_group(pid: i32) -> bool {
    Command::new("taskkill")
        .arg("/T")
        .arg("/F")
        .arg("/PID")
        .arg(pid.to_string())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Stop the background gateway + console started by `ui start`.
fn stop(dir: Option<PathBuf>) -> Result<()> {
    let dir = dir.unwrap_or_else(default_dir);
    let rf = run_file(&dir);
    let Ok(state) = std::fs::read_to_string(&rf) else {
        println!(
            "Nothing to stop (no {} — was `ui start` run?).",
            rf.display()
        );
        return Ok(());
    };
    let mut stopped = 0;
    for line in state.lines() {
        if let Some((name, pid)) = line.split_once('=') {
            if let Ok(pid) = pid.trim().parse::<i32>() {
                if kill_process_group(pid) {
                    println!("✓ stopped {name} (pid {pid})");
                    stopped += 1;
                }
            }
        }
    }
    std::fs::remove_file(&rf).ok();
    if stopped == 0 {
        println!("Nothing was running.");
    }
    Ok(())
}
