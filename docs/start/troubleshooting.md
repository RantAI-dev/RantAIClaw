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

If chat succeeds but persistence fails, the gateway logs a warning and still returns the completed response. Verify the RantaiClaw data directory is writable and that `sessions.db` is not locked by another long-running process.

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
