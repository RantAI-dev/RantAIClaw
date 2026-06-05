//! `rantaiclaw ui` — install and run the optional web console (claw-ui).
//!
//! The console is a separate Next.js app
//! (<https://github.com/RantAI-dev/claw-ui>). It is intentionally NOT bundled in
//! the binary — this command fetches it on demand into `~/.rantaiclaw/ui` and
//! runs it against a local gateway. Everything here shells out to `git` and a
//! JavaScript runtime (`bun`, falling back to `npm`); no JS toolchain is
//! required unless the user opts into the console.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

const REPO_URL: &str = "https://github.com/RantAI-dev/claw-ui.git";
const DEFAULT_GATEWAY: &str = "http://127.0.0.1:3055";
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

/// Pick a JavaScript runtime: prefer `bun`, fall back to `npm`.
fn js_runtime() -> Option<&'static str> {
    if has_binary("bun") {
        Some("bun")
    } else if has_binary("npm") {
        Some("npm")
    } else {
        None
    }
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

pub fn handle_command(command: &crate::UiCommands) -> Result<()> {
    match command {
        crate::UiCommands::Install { dir, r#ref, force } => {
            install(dir.clone(), r#ref.clone(), *force)
        }
        crate::UiCommands::Start { dir, port, gateway } => {
            start(dir.clone(), *port, gateway.clone())
        }
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
    let runtime = js_runtime().context(
        "a JavaScript runtime is required — install bun (https://bun.sh) or Node.js/npm, then retry",
    )?;

    if dir.join(".git").is_dir() {
        println!("↻ Updating existing console at {}", dir.display());
        run(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .arg("pull")
            .arg("--ff-only"))?;
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

    println!("⤓ Installing dependencies with `{runtime}` (this may take a minute) …");
    run(Command::new(runtime).arg("install").current_dir(&dir))?;

    println!("\n✅ Web console installed at {}", dir.display());
    println!("   Start it with:  rantaiclaw ui start");
    Ok(())
}

/// Launch the console's Next.js dev server in the foreground.
fn start(dir: Option<PathBuf>, port: Option<u16>, gateway: Option<String>) -> Result<()> {
    let dir = dir.unwrap_or_else(default_dir);
    if !dir.join("package.json").exists() {
        bail!(
            "web console not installed at {} — run `rantaiclaw ui install` first",
            dir.display()
        );
    }
    let runtime =
        js_runtime().context("a JavaScript runtime is required — install bun or Node.js/npm")?;
    let port = port.unwrap_or(DEFAULT_PORT);
    let gateway = gateway.unwrap_or_else(|| DEFAULT_GATEWAY.to_string());

    // Point the console at the gateway via `.env.local` (mirrors scripts/dev.sh).
    let env_local = format!("RANTAICLAW_GATEWAY_URL={gateway}\nRANTAICLAW_TOKEN=\n");
    std::fs::write(dir.join(".env.local"), env_local)
        .with_context(|| format!("failed to write {}/.env.local", dir.display()))?;

    println!("▶ Web console → http://127.0.0.1:{port}   (gateway: {gateway})");
    println!("  Press Ctrl-C to stop.\n");

    // Invoke Next directly so the port is honoured regardless of the package
    // script's hard-coded `-p`. `bun x next` / `npm exec next` both run the
    // locally-installed Next binary.
    let mut c = Command::new(runtime);
    match runtime {
        "bun" => {
            c.arg("x")
                .arg("next")
                .arg("dev")
                .arg("-p")
                .arg(port.to_string());
        }
        _ => {
            c.arg("exec")
                .arg("--")
                .arg("next")
                .arg("dev")
                .arg("-p")
                .arg(port.to_string());
        }
    }
    c.current_dir(&dir);
    let status = c
        .status()
        .with_context(|| format!("failed to launch `{runtime}` dev server"))?;
    if !status.success() {
        bail!("web console exited with status {status}");
    }
    Ok(())
}
