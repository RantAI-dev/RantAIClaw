# Installing RantaiClaw

The fastest, smallest AI assistant — installed in one line.

This page is the canonical install reference. For a quick overview, see the
README's [Install section](../README.md#install).

---

## TL;DR

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

This downloads the latest pre-built binary for your platform, verifies its
SHA256 checksum, and installs it to `~/.cargo/bin` (or `~/.local/bin` if
cargo isn't present). No Rust toolchain, no compiler, no git clone needed.

After install:

```bash
rantaiclaw --version
rantaiclaw onboard --interactive
rantaiclaw chat
```

---

## Supported pre-built targets

| OS | Architecture | Target tuple |
|---|---|---|
| Linux | x86_64 | `x86_64-unknown-linux-gnu` |
| Linux | aarch64 | `aarch64-unknown-linux-gnu` |
| Linux | armv7 (Raspberry Pi 32-bit) | `armv7-unknown-linux-gnueabihf` |
| macOS | x86_64 (Intel) | `x86_64-apple-darwin` |
| macOS | arm64 (Apple Silicon) | `aarch64-apple-darwin` |
| Windows | x86_64 | `x86_64-pc-windows-msvc` |

Unsupported platform? Use [Build from source](#option-c-build-from-source).

---

## Option A: One-liner (recommended)

### What it does

1. Detects your OS and architecture.
2. Picks the matching archive from the [latest GitHub release](https://github.com/RantAI-dev/RantAIClaw/releases/latest).
3. Downloads `rantaiclaw-<target>.tar.gz` (or `.zip` for Windows).
4. Verifies its SHA256 against the published `SHA256SUMS`.
5. Extracts the binary and installs it to `~/.cargo/bin` or `~/.local/bin`.
6. Tells you exactly how to add the install dir to PATH if needed.

### Standard install

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

### With onboarding right after install

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash -s -- --interactive-onboard
```

### Custom install directory

```bash
RANTAICLAW_INSTALL_DIR=/usr/local/bin curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | sudo -E bash
```

### Skip checksum verification (offline / mirror only)

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | VERIFY_CHECKSUM=false bash
```

### Pin to a specific release

Override the release base URL:

```bash
RANTAICLAW_RELEASE_BASE_URL="https://github.com/RantAI-dev/RantAIClaw/releases/download/v0.4.0" \
  curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

### Reviewing the script before running it

Many shops require this for shell pipes — fair. Save first, read, then run:

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh -o bootstrap.sh
less bootstrap.sh
bash bootstrap.sh
```

---

## Option B: Manual download

For air-gapped environments, security policy, or when you want full control.

### 1. Pick the archive for your platform

From the [latest release](https://github.com/RantAI-dev/RantAIClaw/releases/latest):

```bash
TARGET=x86_64-unknown-linux-gnu     # change to your target
VERSION=v0.4.0                      # or "latest"
curl -fsSL -O "https://github.com/RantAI-dev/RantAIClaw/releases/download/${VERSION}/rantaiclaw-${TARGET}.tar.gz"
curl -fsSL -O "https://github.com/RantAI-dev/RantAIClaw/releases/download/${VERSION}/SHA256SUMS"
```

### 2. Verify the checksum

```bash
grep "rantaiclaw-${TARGET}.tar.gz" SHA256SUMS | sha256sum -c -
# rantaiclaw-x86_64-unknown-linux-gnu.tar.gz: OK
```

On macOS substitute `shasum -a 256 -c -` for `sha256sum -c -`.

### 3. (Optional) Verify the cosign signature

Each release also publishes per-file `.bundle` signatures generated via
keyless [Sigstore](https://www.sigstore.dev/) cosign. To verify:

```bash
curl -fsSL -O "https://github.com/RantAI-dev/RantAIClaw/releases/download/${VERSION}/rantaiclaw-${TARGET}.tar.gz.bundle"
cosign verify-blob \
  --bundle "rantaiclaw-${TARGET}.tar.gz.bundle" \
  --certificate-identity-regexp "https://github.com/RantAI-dev/RantAIClaw/.github/workflows/pub-release.yml@.*" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  "rantaiclaw-${TARGET}.tar.gz"
```

### 4. Extract and install

```bash
tar -xzf "rantaiclaw-${TARGET}.tar.gz"
install -m 0755 rantaiclaw ~/.local/bin/    # or /usr/local/bin with sudo
rantaiclaw --version
```

For Windows: extract the `.zip`, move `rantaiclaw.exe` somewhere on `%PATH%`, run `rantaiclaw.exe --version`.

---

## Option C: Build from source

For contributors, unsupported platforms, or to enable optional feature flags.

### Prerequisites

- Rust toolchain (1.92+) — `rustup` is easiest. The bootstrap script can install it.
- A C toolchain + `pkg-config` + OpenSSL headers + `git` + `curl` + `perl`.
  On Ubuntu: `sudo apt-get install build-essential pkg-config git curl libssl-dev perl`.
- ~6 GB free disk, ~2 GB RAM (release builds are linker-heavy).

### Easiest — let the bootstrap script handle it

```bash
git clone https://github.com/RantAI-dev/RantAIClaw.git
cd RantAIClaw
./bootstrap.sh --from-source --install-system-deps --install-rust
```

`--install-system-deps` and `--install-rust` are no-ops if those are already
present, so it's safe to pass them every time.

### Manual cargo install

```bash
git clone https://github.com/RantAI-dev/RantAIClaw.git
cd RantAIClaw
cargo install --path . --force --locked
```

### Feature flags

```bash
# Default (all common channels + tools)
cargo install --path . --locked

# WhatsApp Web support
cargo install --path . --locked --features whatsapp-web

# Hardware peripherals (RPi GPIO, Arduino)
cargo install --path . --locked --features peripherals

# Browser automation
cargo install --path . --locked --features browser

# Kitchen sink
cargo install --path . --locked --features full
```

See [Cargo.toml](../Cargo.toml) for the full feature list.

---

## Option D: Docker

### Pull the official image

```bash
docker pull ghcr.io/rantai-dev/rantaiclaw:latest
docker run --rm -it ghcr.io/rantai-dev/rantaiclaw:latest --help
```

Multi-arch images are published for `linux/amd64` and `linux/arm64`.

### Persist config + workspace

```bash
mkdir -p ~/.rantaiclaw-docker/{.rantaiclaw,workspace}
docker run --rm -it \
  -v ~/.rantaiclaw-docker/.rantaiclaw:/rantaiclaw-data/.rantaiclaw \
  -v ~/.rantaiclaw-docker/workspace:/rantaiclaw-data/workspace \
  -e HOME=/rantaiclaw-data \
  ghcr.io/rantai-dev/rantaiclaw:latest \
  onboard --interactive
```

### Or let the bootstrap script handle it

```bash
git clone https://github.com/RantAI-dev/RantAIClaw.git
cd RantAIClaw
./bootstrap.sh --docker --interactive-onboard
```

This builds a local image, mounts persistent volumes, and runs onboarding inside the container. Falls back to pulling `ghcr.io/rantai-dev/rantaiclaw:latest` if you pass `--skip-build`.

Podman works too — set `RANTAICLAW_CONTAINER_CLI=podman` or just install only podman and the script auto-detects.

---

## Option E: Homebrew

When the formula is published:

```bash
brew install rantaiclaw
brew upgrade rantaiclaw   # later
```

Tracks the latest tagged release.

---

## Bootstrap script reference

The installer at `scripts/bootstrap.sh` accepts these flags:

```text
Common:
  --guided                 Run the interactive guided installer
  --no-guided              Force non-interactive mode
  --from-source            Build from source instead of downloading a binary
  --no-verify-checksum     Skip SHA256 verification (offline / mirror only)
  --docker                 Build & run inside a container
  --onboard                Run onboarding after install
  --interactive-onboard    Run interactive onboarding (implies --onboard)
  --api-key <key>          API key for non-interactive onboarding
  --provider <id>          Provider for onboarding (default: openrouter)
  --model <id>             Model for onboarding

System bootstrap (only with --from-source):
  --install-system-deps    Install build deps (Linux/macOS)
  --install-rust           Install Rust via rustup if missing

Advanced / build-tuning:
  --build-first            Force separate `cargo build --release --locked` step
  --skip-build             Skip build step
  --skip-install           Skip `cargo install --path . --force --locked`
  --prebuilt-only          Fail if no compatible release asset is available
  --force-source-build     Alias for --from-source
  --prefer-prebuilt        No-op (binary is the default; kept for backward compat)
```

### Environment overrides

| Variable | Default | Purpose |
|---|---|---|
| `RANTAICLAW_RELEASE_BASE_URL` | `https://github.com/RantAI-dev/RantAIClaw/releases/latest/download` | Where to fetch archives + `SHA256SUMS` from |
| `RANTAICLAW_REPO_URL` | `https://github.com/RantAI-dev/RantAIClaw.git` | Git URL for `--from-source` / `--docker` clones |
| `RANTAICLAW_FALLBACK_IMAGE` | `ghcr.io/rantai-dev/rantaiclaw:latest` | Docker fallback image |
| `RANTAICLAW_INSTALL_DIR` | `~/.cargo/bin` (else `~/.local/bin`) | Where the binary lands |
| `VERIFY_CHECKSUM` | `true` | Set to `false` to skip SHA256 verification |
| `RANTAICLAW_API_KEY` | — | Used when `--api-key` not provided |
| `RANTAICLAW_PROVIDER` | `openrouter` | Used when `--provider` not provided |
| `RANTAICLAW_MODEL` | — | Used when `--model` not provided |
| `RANTAICLAW_CONTAINER_CLI` | `docker` (auto-fallback `podman`) | Container CLI for `--docker` |
| `RANTAICLAW_DOCKER_IMAGE` | `rantaiclaw-bootstrap:local` | Local Docker tag |
| `RANTAICLAW_DOCKER_DATA_DIR` | `./.rantaiclaw-docker` | Persistent host path for `--docker` mounts |
| `RANTAICLAW_BOOTSTRAP_MIN_RAM_MB` | `2048` | Source-build preflight RAM threshold |
| `RANTAICLAW_BOOTSTRAP_MIN_DISK_MB` | `6144` | Source-build preflight disk threshold |
| `RANTAICLAW_DISABLE_ALPINE_AUTO_DEPS` | unset | Set to `1` to disable Alpine auto-install |

---

## Update

The installer always installs the latest release. Re-run it to update:

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

For source builds:

```bash
cd RantAIClaw && git pull && cargo install --path . --force --locked
```

---

## Uninstall

```bash
# Binary
rm -f ~/.cargo/bin/rantaiclaw ~/.local/bin/rantaiclaw

# Config + workspace (back up first if you have valuable agent memory)
rm -rf ~/.rantaiclaw
```

For Docker:

```bash
docker rmi ghcr.io/rantai-dev/rantaiclaw:latest rantaiclaw-bootstrap:local
rm -rf ~/.rantaiclaw-docker
```

---

## Verifying the install

```bash
rantaiclaw --version
# rantaiclaw 0.4.0

rantaiclaw status
# checks config, providers, channels, memory backend

rantaiclaw onboard --interactive
# guided provider + autonomy-level setup
```

---

## Troubleshooting

### "command not found: rantaiclaw"

Your install dir isn't in `PATH`. The installer prints the exact line to add
to your shell rc. Or run:

```bash
echo 'export PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

(Use `~/.zshrc` for zsh; for fish, `fish_add_path ~/.cargo/bin ~/.local/bin`.)

### Curl asks for GitHub username/password

This is the classic 404-via-auth-prompt — the script is being told to clone
a repo that doesn't exist. Make sure you're using the URL above (`RantAI-dev/RantAIClaw`).
The current installer never clones on the binary path, so you should not see
this in the default flow.

### "No pre-built binary available for ..."

Your platform isn't in the [supported targets](#supported-pre-built-targets).
Re-run with `--from-source` to build locally.

### Checksum mismatch

The script refuses to install. This either means a corrupt download (retry)
or a mirror serving a stale/tampered archive. **Don't** override with
`VERIFY_CHECKSUM=false` unless you've verified through another channel.

### More

See [docs/troubleshooting.md](troubleshooting.md) for runtime issues
(provider auth, channel disconnects, MCP servers, gateway).

---

## Related

- [Bootstrap one-click reference](one-click-bootstrap.md) — original detail page
- [Configuration reference](config-reference.md)
- [Commands reference](commands-reference.md)
- [Operations runbook](operations-runbook.md)
- [Troubleshooting](troubleshooting.md)
