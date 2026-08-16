# RantaiClaw Troubleshooting

This guide focuses on common setup/runtime failures and fast resolution paths.

Last verified: **July 12, 2026**.

## Installation / Bootstrap

### `cargo` not found

Symptom:

- bootstrap exits with `cargo is not installed`

Fix:

```bash
./bootstrap.sh --install-rust
```

Or install from <https://rustup.rs/>.

### Missing system build dependencies

Symptom:

- build fails due to compiler or `pkg-config` issues

Fix:

```bash
./bootstrap.sh --install-system-deps
```

### Build fails on low-RAM / low-disk hosts

Symptoms:

- `cargo build --release` is killed (`signal: 9`, OOM killer, or `cannot allocate memory`)
- Build crashes after adding swap because disk space runs out

Why this happens:

- Runtime memory (<5MB for common operations) is not the same as compile-time memory.
- Full source build can require **2 GB RAM + swap** and **6+ GB free disk**.
- Enabling swap on a tiny disk can avoid RAM OOM but still fail due to disk exhaustion.

Preferred path for constrained machines:

```bash
./bootstrap.sh --prefer-prebuilt
```

Binary-only mode (no source fallback):

```bash
./bootstrap.sh --prebuilt-only
```

If you must compile from source on constrained hosts:

1. Add swap only if you also have enough free disk for both swap + build output.
1. Limit cargo parallelism:

```bash
CARGO_BUILD_JOBS=1 cargo build --release --locked
```

1. Reduce heavy features when Matrix is not required:

```bash
cargo build --release --locked --features hardware
```

1. Cross-compile on a stronger machine and copy the binary to the target host.

### Build is very slow or appears stuck

Symptoms:

- `cargo check` / `cargo build` appears stuck at `Checking rantaiclaw` for a long time
- repeated `Blocking waiting for file lock on package cache` or `build directory`

Why this happens in RantaiClaw:

- Matrix E2EE stack (`matrix-sdk`, `ruma`, `vodozemac`) is large and expensive to type-check.
- TLS + crypto native build scripts (`aws-lc-sys`, `ring`) add noticeable compile time.
- `rusqlite` with bundled SQLite compiles C code locally.
- Running multiple cargo jobs/worktrees in parallel causes lock contention.

Fast checks:

```bash
cargo check --timings
cargo tree -d
```

The timing report is written to `target/cargo-timings/cargo-timing.html`.

Faster local iteration (when Matrix channel is not needed):

```bash
cargo check
```

This uses the lean default feature set and can significantly reduce compile time.

To build with Matrix support explicitly enabled:

```bash
cargo check --features channel-matrix
```

To build with Matrix + Lark + hardware support:

```bash
cargo check --features hardware,channel-matrix,channel-lark
```

Lock-contention mitigation:

```bash
pgrep -af "cargo (check|build|test)|cargo check|cargo build|cargo test"
```

Stop unrelated cargo jobs before running your own build.

### `rantaiclaw` command not found after install

Symptom:

- install succeeds but shell cannot find `rantaiclaw`

Fix:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
which rantaiclaw
```

Persist in your shell profile if needed.

## Runtime / Gateway

### Startup fails with `openai: OPENAI_API_KEY required` (or anthropic / gemini)

Cause: the config names a provider that cannot start without an API key, and
no key is stored or exported. The usual producer was setup on **v0.20.0-alpha
or earlier**: choosing such a provider and leaving the key prompt empty saved
the pair anyway.

On binaries **after v0.20.0-alpha** this state no longer aborts: the TUI opens,
reports the failure, and drops you into provider setup — enter a key or switch
provider and the session heals in place. Setup also refuses to save the broken
combination in the first place.

On **v0.20.0-alpha and earlier** the same config kills every launch, including
`rantaiclaw setup`. Two manual escapes:

1. Export the provider's key for the session, then repair via setup:

```bash
OPENAI_API_KEY=sk-... rantaiclaw setup provider
```

(PowerShell: `$env:OPENAI_API_KEY="sk-..."; rantaiclaw setup provider`)

2. Or edit `~/.rantaiclaw/profiles/<name>/config.toml` directly: set
   `default_provider` to a provider that runs keyless (for example
   `"openrouter"` or `"ollama"`), save, and relaunch.

### Gateway unreachable

Checks:

```bash
rantaiclaw status
rantaiclaw doctor
```

Verify `~/.rantaiclaw/config.toml`:

- `[gateway].host` (default `127.0.0.1`)
- `[gateway].port` (default `9393`)
- `allow_public_bind` only when intentionally exposing LAN/public interfaces

### Pairing / auth failures on webhook

Checks:

1. Ensure pairing completed (`/pair` flow)
2. Ensure bearer token is current
3. Re-run diagnostics:

```bash
rantaiclaw doctor
```

### API chat succeeds but no session appears

Checks:

```bash
rantaiclaw session list
curl -s http://127.0.0.1:9393/api/v1/sessions
```

Expected behavior:

- `POST /api/v1/agent/chat` records a completed turn with `source = "api"`.
- The session contains the user message, assistant response, derived title, and end timestamp.

If chat succeeds but persistence fails, the gateway logs a warning and still returns the completed response. Verify the RantaiClaw data directory is writable and that the active profile's `sessions.db` (`~/.rantaiclaw/profiles/<name>/sessions/sessions.db`) is not locked by another long-running process.

### `skills install-deps` download extraction fails

Checks:

```bash
rantaiclaw skills list
rantaiclaw skills install-deps <skill>
which tar
which unzip
```

`download` recipes use system `tar` for `tar.gz`/`tgz` archives and system `unzip` for `zip` archives. Extraction is rejected if archive entries use absolute paths or `..` traversal. For a rejected archive, inspect the skill with:

```bash
rantaiclaw skills inspect <slug>
```

### TUI says "copied N lines" but the clipboard is empty

The TUI copies drag-selected chat text with Ctrl+C using OSC 52, a terminal
escape sequence — the app cannot confirm delivery, so an unsupporting terminal
fails silently.

Terminal support in one line: Windows Terminal, iTerm2, kitty, alacritty,
WezTerm, and tmux ≥ 3.3 (`set -g set-clipboard on`) implement OSC 52;
GNOME Terminal and other VTE-based terminals do not.

On an unsupporting terminal, use native selection instead: hold **Shift**
while dragging (Option on iTerm2) — the terminal bypasses the TUI's mouse
capture and its normal copy shortcut works. Native selection copies the
screen as-is, so the pane border `│` and wrapped line breaks come along.

## Channel Issues

### Telegram conflict: `terminated by other getUpdates request`

Cause:

- multiple pollers using same bot token

Fix:

- keep only one active runtime for that token
- stop extra `rantaiclaw daemon` / `rantaiclaw channel start` processes

### Channel unhealthy in `channel doctor`

Checks:

```bash
rantaiclaw channel doctor
```

Then verify channel-specific credentials + allowlist fields in config.

### Channel connects, `channel doctor` passes, but nothing arrives

For the three HMAC-verified webhook channels, a **missing inbound secret
disables the endpoint** — the gateway returns `401` before parsing the body, so
the channel looks healthy from every other angle:

| Channel | Required secret | Log line |
|---|---|---|
| WhatsApp (Cloud API) | `app_secret` / `RANTAICLAW_WHATSAPP_APP_SECRET` | `WhatsApp webhook rejected: no app secret configured.` |
| Linq | `signing_secret` / `RANTAICLAW_LINQ_SIGNING_SECRET` | `Linq webhook rejected: no signing secret configured.` |
| Nextcloud Talk | `webhook_secret` / `RANTAICLAW_NEXTCLOUD_TALK_WEBHOOK_SECRET` | `Nextcloud Talk webhook rejected: no webhook secret configured.` |

See [Channels reference §2](../reference/channels.md#2-delivery-modes-at-a-glance)
for which channels have an inbound endpoint at all.

### The agent replies but refuses every privileged tool

This is the approval model, not the transport. With no `approval_owners`
configured, nobody can approve, so approval-required tools auto-deny. Add an
owner — do not reach for `autonomous_tools = true`, which removes the gate for
everyone on the channel.

See [Per-role channel permissions](../security/per-role-permissions.md) and
[Channels reference §4b](../reference/channels.md#4b-approval-and-roles).

## Service Mode

### Service installed but not running

Checks:

```bash
rantaiclaw service status
```

Recovery:

```bash
rantaiclaw service stop
rantaiclaw service start
```

Linux logs:

```bash
journalctl --user -u rantaiclaw.service -f
```

### Agent can't find a CLI tool (e.g. `kubectl`) as a service, but it worked in the TUI

Symptom: a shell command the agent runs fine when you launch `rantaiclaw` interactively
reports "not found" / "not set up" once you run it via `rantaiclaw service install` + `start`.
Provider keys, Telegram owner, and other config-stored settings still work — only the shell
tool is affected.

Cause: a systemd `--user` service starts with a minimal `PATH` that omits user-local bin
directories (`~/.local/bin`, `~/.cargo/bin`, `~/.nvm/…`) where operator-installed tools like
`kubectl` usually live. Your interactive shell has them; the bare service does not.

Fix: `service install` captures your current working directory and `PATH` into the unit, so
reinstalling from a shell where the tool is on `PATH` resolves it:

```bash
which kubectl                 # confirm the tool is on your interactive PATH
rantaiclaw service install    # re-run from that shell to capture PATH + working directory
rantaiclaw service start
```

Or add a drop-in without reinstalling:

```bash
systemctl --user edit rantaiclaw.service
# [Service]
# Environment=PATH=/home/<you>/.local/bin:/usr/local/bin:/usr/bin:/bin:/snap/bin
# WorkingDirectory=/home/<you>/<your-project>
systemctl --user restart rantaiclaw.service
```

Caveat: the shell tool forwards only an allowlist of environment variables. A default
`~/.kube/config` is found via `HOME` and works; a custom `KUBECONFIG` path is **not** currently
forwarded to shell commands — point kubectl at the default location or symlink your config there.

## Web Console (`ui`)

### `ui start` says node is required

Symptom:

- `ui start` exits with `` `node` is required to run the web console (Node >= 18.18) — install Node.js and retry ``

Why this happens:

- `ui install` downloads a signed prebuilt claw-ui release and `ui start` serves it directly with `node server.js` — there is no on-machine `npm`/`bun` build step anymore, so Node.js itself is the only runtime prerequisite left.

Fix:

- Install Node.js **18.18+** (20 LTS recommended) from <https://nodejs.org/> or your package manager, then re-run:

```bash
rantaiclaw ui start
```

### `ui install` refuses to verify the release

Symptom:

- `ui install` exits with a SHA256 mismatch, or `no cosign signature published for <tag> — refusing to install an unsigned console artifact (possible tampering)`

Why this happens:

- `ui install` verifies SHA256 then cosign, in that order, before extracting anything. claw-ui is signed from its first release, so — unlike the binary self-updater, which tolerates missing signatures on releases published before it started signing — a missing cosign bundle fails closed here.

Fix:

- Do not bypass this check. Confirm you're pulling the intended `--ref` (release tag) and that no proxy/mirror is altering the download. If `cosign` itself isn't installed locally, `ui install` only warns and continues with SHA-only verification — install cosign (<https://docs.sigstore.dev/system_config/installation/>) for the full guarantee.

### Every panel says `Gateway unreachable … — unexpected_host`

Symptom:

- The console loads, but chat and every panel show `Gateway unreachable. Start the agent gateway, then retry — unexpected_host.` Restarting the gateway changes nothing. Typically appears right after a UI upgrade, when the console is opened via a LAN address or a DNS name instead of `localhost`.

Why this happens:

- The 403 comes from the console's own request proxy, not from the gateway. claw-ui v0.3.18 added a `Host`-header allowlist to block DNS-rebinding attacks, and its default only covered `localhost`/loopback — so a console opened at `http://<lan-ip>:3939` was silently locked out.

Fix:

- Upgrade the console to claw-ui **v0.3.19** or later (`rantaiclaw ui update`): IP-literal hosts (LAN addresses included) are always served, no configuration needed. An IP in the `Host` header cannot be a DNS-rebinding vector — the attack requires a DNS name.
- Only if you reach the console by a **DNS name** (a tunnel domain, `console.lan`): list it explicitly, then restart the console:

```bash
RANTAICLAW_UI_ALLOWED_HOSTS=console.lan rantaiclaw ui start
```

## Legacy Installer Compatibility

Both still work:

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/install.sh | bash
```

`install.sh` is a compatibility entry and forwards/falls back to bootstrap behavior.

## Still Stuck?

Collect and include these outputs when filing an issue:

```bash
rantaiclaw --version
rantaiclaw status
rantaiclaw doctor
rantaiclaw channel doctor
```

Also include OS, install method, and sanitized config snippets (no secrets).

## Related Docs

- [operations-runbook.md](../operations/runbook.md)
- [one-click-bootstrap.md](one-click-bootstrap.md)
- [channels-reference.md](../reference/channels.md)
- [network-deployment.md](../operations/network-deployment.md)
