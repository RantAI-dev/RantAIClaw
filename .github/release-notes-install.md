## Install RantaiClaw __VERSION__

Pick your platform. Every artifact below is SHA256-checksummed (`SHA256SUMS`) and cosign-signed (`.bundle` files alongside).

<details open>
<summary><strong>macOS</strong> &mdash; Intel + Apple Silicon</summary>

### One-liner (auto-detects Intel vs Apple Silicon)

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

### Manual

| Mac | Artifact |
|---|---|
| Apple Silicon (M1 / M2 / M3 / M4) | [`rantaiclaw-aarch64-apple-darwin.tar.gz`](https://github.com/RantAI-dev/RantAIClaw/releases/download/__VERSION__/rantaiclaw-aarch64-apple-darwin.tar.gz) |
| Intel | [`rantaiclaw-x86_64-apple-darwin.tar.gz`](https://github.com/RantAI-dev/RantAIClaw/releases/download/__VERSION__/rantaiclaw-x86_64-apple-darwin.tar.gz) |

```bash
# Apple Silicon example
curl -fsSLO https://github.com/RantAI-dev/RantAIClaw/releases/download/__VERSION__/rantaiclaw-aarch64-apple-darwin.tar.gz
tar -xzf rantaiclaw-aarch64-apple-darwin.tar.gz
xattr -dr com.apple.quarantine rantaiclaw   # bypass Gatekeeper (binary is not yet Apple-notarized)
sudo install -m 755 rantaiclaw /usr/local/bin/
rantaiclaw --version
```

> Homebrew formula is planned, not yet published.

</details>

<details>
<summary><strong>Linux</strong> &mdash; Ubuntu / Debian / Arch / Fedora / RHEL / openSUSE / Pi</summary>

### One-liner (works on every major distro)

```bash
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

The binary is dynamically linked against glibc and works on Ubuntu/Debian/Mint/Pop!_OS, Arch/Manjaro/EndeavourOS, Fedora/RHEL/Rocky/Alma, openSUSE, and Alpine (with `gcompat`).

### Architecture matrix

| Arch | Artifact |
|---|---|
| x86_64 | [`rantaiclaw-x86_64-unknown-linux-gnu.tar.gz`](https://github.com/RantAI-dev/RantAIClaw/releases/download/__VERSION__/rantaiclaw-x86_64-unknown-linux-gnu.tar.gz) |
| aarch64 (64-bit ARM &mdash; Pi 4/5, ARM servers, Ampere) | [`rantaiclaw-aarch64-unknown-linux-gnu.tar.gz`](https://github.com/RantAI-dev/RantAIClaw/releases/download/__VERSION__/rantaiclaw-aarch64-unknown-linux-gnu.tar.gz) |
| armv7 (32-bit ARM &mdash; Pi 2/3, BeagleBone) | [`rantaiclaw-armv7-unknown-linux-gnueabihf.tar.gz`](https://github.com/RantAI-dev/RantAIClaw/releases/download/__VERSION__/rantaiclaw-armv7-unknown-linux-gnueabihf.tar.gz) |

### Manual install

```bash
# x86_64 example &mdash; same recipe on Ubuntu/Debian, Arch, Fedora, openSUSE
curl -fsSLO https://github.com/RantAI-dev/RantAIClaw/releases/download/__VERSION__/rantaiclaw-x86_64-unknown-linux-gnu.tar.gz
tar -xzf rantaiclaw-x86_64-unknown-linux-gnu.tar.gz
sudo install -m 755 rantaiclaw /usr/local/bin/
rantaiclaw --version
```

> Native `.deb` and AUR packages are not yet published &mdash; use the tarball above. Tracking issues in the repo.

</details>

<details>
<summary><strong>Windows</strong> &mdash; WSL2 (recommended) or native</summary>

### WSL2 (recommended)

```bash
# Inside any WSL2 distro (Ubuntu, Debian, Arch, ...)
curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash
```

### Native PowerShell

Artifact: [`rantaiclaw-x86_64-pc-windows-msvc.zip`](https://github.com/RantAI-dev/RantAIClaw/releases/download/__VERSION__/rantaiclaw-x86_64-pc-windows-msvc.zip)

```powershell
$ver = "__VERSION__"
Invoke-WebRequest "https://github.com/RantAI-dev/RantAIClaw/releases/download/$ver/rantaiclaw-x86_64-pc-windows-msvc.zip" -OutFile rantaiclaw.zip
Expand-Archive rantaiclaw.zip -DestinationPath "$Env:USERPROFILE\rantaiclaw"

# Current shell PATH
$Env:PATH = "$Env:USERPROFILE\rantaiclaw;$Env:PATH"

# Persistent (User scope)
[Environment]::SetEnvironmentVariable("PATH", "$Env:USERPROFILE\rantaiclaw;$([Environment]::GetEnvironmentVariable('PATH','User'))", "User")

rantaiclaw --version
```

</details>

<details>
<summary><strong>Docker</strong> &mdash; linux/amd64 + linux/arm64</summary>

```bash
# Pull the exact release
docker pull ghcr.io/rantai-dev/rantaiclaw:__VERSION__

# Or :latest for the most recent release
docker pull ghcr.io/rantai-dev/rantaiclaw:latest

# Run interactively with persistent config and workspace
docker run --rm -it \
  -v "$HOME/.rantaiclaw:/root/.rantaiclaw" \
  ghcr.io/rantai-dev/rantaiclaw:__VERSION__ \
  chat
```

</details>

<details>
<summary><strong>From source</strong> &mdash; any platform with Rust 1.80+</summary>

```bash
# Cargo install (no clone)
cargo install --git https://github.com/RantAI-dev/RantAIClaw --tag __VERSION__ --locked

# Or clone and use the bundled bootstrap
git clone --depth 1 --branch __VERSION__ https://github.com/RantAI-dev/RantAIClaw.git
cd RantAIClaw
./bootstrap.sh --from-source
```

</details>

<details>
<summary><strong>Verify the download</strong> &mdash; SHA256 + cosign</summary>

```bash
# SHA256
curl -fsSLO https://github.com/RantAI-dev/RantAIClaw/releases/download/__VERSION__/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing

# cosign (sigstore bundle &mdash; keyless verification of OIDC identity)
cosign verify-blob \
  --bundle rantaiclaw-x86_64-unknown-linux-gnu.tar.gz.bundle \
  --certificate-identity-regexp 'https://github.com/RantAI-dev/RantAIClaw' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  rantaiclaw-x86_64-unknown-linux-gnu.tar.gz
```

</details>

### First run

```bash
rantaiclaw --version
rantaiclaw setup     # guided wizard &mdash; provider, approvals, channels, persona, skills, MCP
rantaiclaw doctor    # validate the install
rantaiclaw chat      # launch the TUI and start chatting
```

📖 Full reference: [README](https://github.com/RantAI-dev/RantAIClaw#install) · [`docs/install.md`](https://github.com/RantAI-dev/RantAIClaw/blob/main/docs/install.md) · [Troubleshooting](https://github.com/RantAI-dev/RantAIClaw/blob/main/docs/troubleshooting.md)
