//! `rantaiclaw ui` — install and run the optional web console (claw-ui).
//!
//! The console is a separate Next.js app
//! (<https://github.com/RantAI-dev/claw-ui>). It is intentionally NOT bundled in
//! the binary — this command fetches it on demand into `~/.rantaiclaw/ui` and
//! runs it against a local gateway. Everything here shells out to `git` and a
//! JavaScript runtime (`bun`, falling back to `npm`); no JS toolchain is
//! required unless the user opts into the console.

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};

const REPO_URL: &str = "https://github.com/RantAI-dev/claw-ui.git";
const DEFAULT_PORT: u16 = 3939;

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

/// True when `cmd` resolves on PATH. Use for tools without a clean `--version`, e.g. `unzip`.
fn has_command(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A JavaScript runtime and how to invoke it (`bun` preferred; `npm` is the fallback).
enum JsRuntime {
    Bun(String),
    Npm(String),
}

impl JsRuntime {
    /// Command (or absolute path) used to invoke the runtime.
    fn cmd(&self) -> &str {
        match self {
            JsRuntime::Bun(c) | JsRuntime::Npm(c) => c,
        }
    }
    /// Short name for log lines.
    fn name(&self) -> &str {
        match self {
            JsRuntime::Bun(_) => "bun",
            JsRuntime::Npm(_) => "npm",
        }
    }
}

/// Where bun lands when installed via <https://bun.sh/install>: `$BUN_INSTALL/bin/bun`
/// (default `$HOME/.bun/bin/bun`).
fn bun_default_path() -> PathBuf {
    let base = std::env::var_os("BUN_INSTALL")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".bun")))
        .unwrap_or_else(|| PathBuf::from(".bun"));
    base.join("bin").join("bun")
}

/// Find an existing runtime: `bun` on PATH, a bun at its default install path, then `npm`.
fn detect_js_runtime() -> Option<JsRuntime> {
    if has_binary("bun") {
        return Some(JsRuntime::Bun("bun".into()));
    }
    let bun = bun_default_path();
    if bun.is_file()
        && Command::new(&bun)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    {
        return Some(JsRuntime::Bun(bun.to_string_lossy().into_owned()));
    }
    if has_binary("npm") {
        return Some(JsRuntime::Npm("npm".into()));
    }
    None
}

/// Ensure `unzip` is present — bun's installer uses it to extract its release. Installs it via
/// the system package manager (with `sudo` when not root) if missing.
fn ensure_unzip() -> Result<()> {
    if has_command("unzip") {
        return Ok(());
    }
    println!("⤓ bun needs `unzip` to unpack its release — installing unzip …");
    let is_root = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false);
    let sudo = if is_root || !has_command("sudo") {
        ""
    } else {
        "sudo "
    };
    let managers: [(&str, String); 6] = [
        (
            "apt-get",
            format!("{sudo}apt-get update && {sudo}apt-get install -y unzip"),
        ),
        ("dnf", format!("{sudo}dnf install -y unzip")),
        ("yum", format!("{sudo}yum install -y unzip")),
        ("apk", format!("{sudo}apk add --no-cache unzip")),
        ("pacman", format!("{sudo}pacman -Sy --noconfirm unzip")),
        ("zypper", format!("{sudo}zypper install -y unzip")),
    ];
    for (mgr, cmd) in &managers {
        if has_command(mgr) {
            let _ = Command::new("bash").arg("-c").arg(cmd).status();
            if has_command("unzip") {
                return Ok(());
            }
        }
    }
    bail!("`unzip` is required to install bun but couldn't be installed automatically — install unzip (e.g. `apt-get install -y unzip`) and retry");
}

/// Auto-install bun via its official installer (needs `curl` + `bash`; uses `unzip`).
fn install_bun() -> Result<JsRuntime> {
    if !has_binary("curl") {
        bail!("a JavaScript runtime is required and `curl` isn't available to auto-install bun — install bun (https://bun.sh) or Node.js/npm, then retry");
    }
    ensure_unzip()?; // bun's installer unzips its release — make sure unzip exists first
    println!("⤓ No JavaScript runtime found — installing bun (https://bun.sh) …");
    let status = Command::new("bash")
        .arg("-c")
        .arg("curl -fsSL https://bun.sh/install | bash")
        .status()
        .context("failed to run the bun installer")?;
    if !status.success() {
        bail!(
            "bun install failed — install bun (https://bun.sh) or Node.js/npm manually, then retry"
        );
    }
    let bun = bun_default_path();
    if bun.is_file() {
        println!("✓ bun installed → {}", bun.display());
        Ok(JsRuntime::Bun(bun.to_string_lossy().into_owned()))
    } else {
        bail!(
            "bun installed but {} was not found — open a new shell (so bun is on PATH) and retry",
            bun.display()
        );
    }
}

/// Get a JS runtime, auto-installing bun when neither bun nor npm is present.
fn ensure_js_runtime() -> Result<JsRuntime> {
    match detect_js_runtime() {
        Some(rt) => Ok(rt),
        None => install_bun(),
    }
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

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn {cmd:?}"))?;
    if !status.success() {
        bail!("command failed ({status}): {cmd:?}");
    }
    Ok(())
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

/// Clone (or update) claw-ui and install its dependencies.
fn install(dir: Option<PathBuf>, git_ref: Option<String>, force: bool) -> Result<()> {
    let dir = dir.unwrap_or_else(default_dir);

    if !has_binary("git") {
        bail!("`git` is required to install the web console — install git and retry");
    }
    let runtime = ensure_js_runtime()?;

    if dir.join(".git").is_dir() {
        println!("↻ Updating existing console at {}", dir.display());
        // This checkout is managed by rantaiclaw, not meant for hand edits. The
        // JS runtime (`bun`/`npm`) rewrites the lockfile during install, leaving
        // the tree dirty; discard that churn first so the fast-forward never
        // aborts on "you have unstaged changes".
        run(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["checkout", "--", "."]))?;
        // `-c pull.rebase=false` neutralizes a user's global `pull.rebase=true`,
        // which would otherwise turn `--ff-only` into a rebase that refuses to
        // run with a dirty tree.
        run(Command::new("git").arg("-C").arg(&dir).args([
            "-c",
            "pull.rebase=false",
            "pull",
            "--ff-only",
        ]))?;
    } else {
        let non_empty = dir
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if non_empty {
            if !force {
                bail!(
                    "{} exists and is not empty — pass --force to overwrite",
                    dir.display()
                );
            }
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to clear {}", dir.display()))?;
        }
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        println!("⤓ Cloning {REPO_URL} → {}", dir.display());
        let mut c = Command::new("git");
        c.arg("clone").arg("--depth").arg("1");
        if let Some(r) = &git_ref {
            c.arg("--branch").arg(r);
        }
        c.arg(REPO_URL).arg(&dir);
        run(&mut c)?;
    }

    println!(
        "⤓ Installing dependencies with `{}` (this may take a minute) …",
        runtime.name()
    );
    run(Command::new(runtime.cmd()).arg("install").current_dir(&dir))?;

    println!("\n✅ Web console installed at {}", dir.display());
    println!("   Start it with:  rantaiclaw ui start");
    Ok(())
}

/// Bring up the console: ensure the gateway is running (auto-start + auto-pair if needed) and
/// launch the Next.js dev server — both in the background. Tear down with `rantaiclaw ui stop`.
fn start(
    dir: Option<PathBuf>,
    port: Option<u16>,
    gateway: Option<String>,
    token: Option<String>,
    config: &crate::config::Config,
) -> Result<()> {
    let dir = dir.unwrap_or_else(default_dir);
    if !dir.join("package.json").exists() {
        bail!(
            "web console not installed at {} — run `rantaiclaw ui install` first",
            dir.display()
        );
    }
    let runtime = ensure_js_runtime()?;
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
        println!("  Stop it with: rantaiclaw ui stop");
        return Ok(());
    }

    // 1. Ensure the gateway the console talks to is actually up.
    let (gw_host, gw_port) = split_host_port(&gateway);
    let gw_log = dir.join("gateway.log");
    let mut gateway_pid: Option<u32> = None;
    if port_open(&gw_host, gw_port) {
        println!("✓ gateway already running ({gateway})");
    } else {
        println!("▶ starting gateway ({gateway}) …");
        let exe =
            std::env::current_exe().context("could not resolve the rantaiclaw binary path")?;
        let mut g = Command::new(&exe);
        g.arg("gateway")
            .arg("-p")
            .arg(gw_port.to_string())
            .arg("--host")
            .arg(&gw_host);
        gateway_pid = Some(spawn_detached(&mut g, &gw_log)?);
        if !wait_for_port(&gw_host, gw_port, 20) {
            bail!(
                "gateway did not come up on {gw_host}:{gw_port} — see {}",
                gw_log.display()
            );
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

    // Point the console at the gateway via `.env.local` (gateway URL + resolved token).
    let env_local = format!(
        "RANTAICLAW_GATEWAY_URL={gateway}\nRANTAICLAW_TOKEN={token}\nRANTAICLAW_UI_SECRET={ui_secret}\n"
    );
    std::fs::write(&env_path, env_local)
        .with_context(|| format!("failed to write {}/.env.local", dir.display()))?;

    // 2. Launch the Next.js dev server in the background. Invoke Next directly so the port is
    //    honoured regardless of the package script's hard-coded `-p`.
    println!("▶ starting web console …");
    let ui_log = dir.join("ui.log");
    let mut c = Command::new(runtime.cmd());
    match &runtime {
        JsRuntime::Bun(_) => {
            c.arg("x")
                .arg("next")
                .arg("dev")
                .arg("-p")
                .arg(port.to_string());
        }
        JsRuntime::Npm(_) => {
            c.arg("exec")
                .arg("--")
                .arg("next")
                .arg("dev")
                .arg("-p")
                .arg(port.to_string());
        }
    }
    c.current_dir(&dir);
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
    println!("  logs: {} · {}", ui_log.display(), gw_log.display());
    println!("  stop: rantaiclaw ui stop");
    Ok(())
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
