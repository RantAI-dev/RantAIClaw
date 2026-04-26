# One-Click Bootstrap

This page defines the fastest supported path to install and initialize RantaiClaw.

Last verified: **April 26, 2026**.

## Option 0: Homebrew (macOS/Linuxbrew)

```bash
brew install rantaiclaw
```

## Option A (Recommended): Remote one-liner

The default installer downloads the **latest pre-built binary** for your
platform, verifies its SHA256 checksum, and installs it. No Rust toolchain,
no compiler, no git clone.

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

Legacy compatibility entrypoint:

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/install.sh | bash
```

For high-security environments, prefer Option B so you can review the script
before execution.

## Option B: Clone + local script

```bash
git clone https://github.com/RantAI-dev/RantAIClaw.git
cd RantAIClaw
./bootstrap.sh
```

The default flow still downloads the pre-built binary. To build from source
instead (contributor workflow, or unsupported platform):

```bash
./bootstrap.sh --from-source
```

That runs `cargo build --release --locked` then `cargo install --path . --force --locked`.

## Supported pre-built targets

The installer auto-detects your platform and downloads the matching archive
from [the latest GitHub release](https://github.com/RantAI-dev/RantAIClaw/releases/latest):

| OS / arch | Target tuple |
|---|---|
| Linux x86_64 | `x86_64-unknown-linux-gnu` |
| Linux aarch64 | `aarch64-unknown-linux-gnu` |
| Linux armv7 | `armv7-unknown-linux-gnueabihf` |
| macOS x86_64 | `x86_64-apple-darwin` |
| macOS arm64 | `aarch64-apple-darwin` |
| Windows x86_64 | `x86_64-pc-windows-msvc` |

Unsupported platform? Re-run with `--from-source`.

## Source-build resource notes

If you opt into `--from-source`, expect:

- **2 GB RAM + swap**
- **6 GB free disk**

The installer prints a preflight warning when these thresholds aren't met,
but does not auto-switch back to binary install — pass `--from-source`
explicitly only when you mean it.

## Installer flags

For the full reference, run:

```bash
./bootstrap.sh --help
```

Highlights:

- `--from-source` — build from source instead of downloading a binary (default is binary)
- `--no-verify-checksum` — skip SHA256 verification (offline / mirror only)
- `--guided` — interactive guided installer
- `--docker` — build & run inside a container
- `--onboard` / `--interactive-onboard` — connect a provider after install
- `--api-key`, `--provider`, `--model` — non-interactive onboarding values
- `--install-system-deps`, `--install-rust` — only relevant with `--from-source`

Backward-compatible (deprecated) aliases:

- `--prefer-prebuilt` — no-op (binary is the default now)
- `--prebuilt-only` — fail if no compatible release asset is published
- `--force-source-build` — alias for `--from-source`

## Optional onboarding modes

### Containerized onboarding (Docker)

```bash
./bootstrap.sh --docker
```

This builds a local RantaiClaw image and launches onboarding inside a container while
persisting config/workspace to `./.rantaiclaw-docker`.

Container CLI defaults to `docker`. If Docker CLI is unavailable and `podman` exists,
bootstrap auto-falls back to `podman`. You can also set `RANTAICLAW_CONTAINER_CLI`
explicitly (for example: `RANTAICLAW_CONTAINER_CLI=podman ./bootstrap.sh --docker`).

For Podman, bootstrap runs with `--userns keep-id` and `:Z` volume labels so
workspace/config mounts remain writable inside the container.

If you add `--skip-build`, bootstrap skips local image build. It first tries the local
Docker tag (`RANTAICLAW_DOCKER_IMAGE`, default: `rantaiclaw-bootstrap:local`); if missing,
it pulls `ghcr.io/rantai-dev/rantaiclaw:latest` and tags it locally before running.

### Quick onboarding (non-interactive)

```bash
./bootstrap.sh --onboard --api-key "sk-..." --provider openrouter
```

Or with environment variables:

```bash
RANTAICLAW_API_KEY="sk-..." RANTAICLAW_PROVIDER="openrouter" ./bootstrap.sh --onboard
```

### Interactive onboarding

```bash
./bootstrap.sh --interactive-onboard
```

## Environment variables

| Variable | Purpose |
|---|---|
| `RANTAICLAW_RELEASE_BASE_URL` | Override release-archive base URL (mirror / staging) |
| `RANTAICLAW_REPO_URL` | Override git URL for `--from-source` / `--docker` clones |
| `RANTAICLAW_FALLBACK_IMAGE` | Override fallback Docker image |
| `RANTAICLAW_INSTALL_DIR` | Override install directory (default: `~/.cargo/bin`, else `~/.local/bin`) |
| `VERIFY_CHECKSUM` | Set to `false` to skip SHA256 check |
| `RANTAICLAW_API_KEY` | Used when `--api-key` is not provided |
| `RANTAICLAW_PROVIDER` | Used when `--provider` is not provided |
| `RANTAICLAW_MODEL` | Used when `--model` is not provided |

## Related docs

- [README.md](../README.md)
- [commands-reference.md](commands-reference.md)
- [providers-reference.md](providers-reference.md)
- [channels-reference.md](channels-reference.md)
- [troubleshooting.md](troubleshooting.md)
