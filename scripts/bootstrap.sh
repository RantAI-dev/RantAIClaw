#!/usr/bin/env bash
set -euo pipefail

__BOOTSTRAP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" >/dev/null 2>&1 && pwd || pwd)"

# Source UX helpers; fall back to minimal definitions if missing
# (defensive — partial checkout, network-fetched single file, etc.).
if [[ -r "$__BOOTSTRAP_DIR/lib/ui.sh" ]]; then
  # shellcheck source=lib/ui.sh
  source "$__BOOTSTRAP_DIR/lib/ui.sh"
else
  info()    { printf '==> %s\n' "$*"; }
  success() { printf 'OK %s\n' "$*"; }
  warn()    { printf 'warning: %s\n' "$*" >&2; }
  error()   { printf 'error: %s\n' "$*" >&2; }
  step()    { printf '\n[%s] %s\n' "$1" "${*:2}"; }
  print_banner()         { printf '== RantaiClaw Installer ==\n'; }
  print_success_banner() { printf '== Installation Complete ==\n'; for s in "$@"; do printf '  - %s\n' "$s"; done; }
  spinner_start()       { info "$*…"; }
  spinner_stop()        { success "$*"; }
  spinner_stop_fail()   { error "$*"; }
  IS_INTERACTIVE=true
  [[ ! -t 0 ]] && IS_INTERACTIVE=false
  prompt_yes_no() { local d="${2:-yes}"; case "$d" in [yY]*|1|true) return 0 ;; *) return 1 ;; esac; }
  prompt_input() { printf '%s' "${2:-}"; }
  prompt_input_secret() { printf ''; }
fi


usage() {
  cat <<'USAGE'
RantaiClaw installer

Usage:
  ./rantaiclaw_install.sh [options]
  ./bootstrap.sh [options]         # compatibility entrypoint

Default behavior:
  Downloads the latest pre-built release binary for your platform, verifies
  its SHA256 checksum, and installs it. No Rust toolchain, no compiler, no
  git clone needed — beginner-friendly out of the box.

  Pass --from-source to build from source instead (requires Rust toolchain).
  Pass --docker to run inside a container.

Common options:
  -h, --help                 Show this help
  --guided                   Run the interactive guided installer
  --no-guided                Force non-interactive mode
  --from-source              Build from source instead of downloading a binary
  --no-verify-checksum       Skip SHA256 verification (offline / mirror only)
  --docker                   Build & run inside a container
  --onboard                  Run onboarding after install
  --interactive-onboard      Run interactive onboarding (implies --onboard)
  --api-key <key>            API key for non-interactive onboarding
  --provider <id>            Provider for non-interactive onboarding (default: openrouter)
  --model <id>               Model for non-interactive onboarding (optional)

System bootstrap (only with --from-source):
  --install-system-deps      Install build dependencies (Linux/macOS)
  --install-rust             Install Rust via rustup if missing

Advanced / build-tuning:
  --build-first              Force separate `cargo build --release --locked` step
  --skip-build               Skip build step
  --skip-install             Skip `cargo install --path . --force --locked`
  --prefer-prebuilt          Deprecated alias (binary is already the default)
  --prebuilt-only            Fail if no compatible release asset is available
  --force-source-build       Alias for --from-source

Examples:
  # Beginner-friendly one-liner (binary install, checksum verified):
  curl -fsSL https://raw.githubusercontent.com/RantAI-dev/RantAIClaw/main/scripts/bootstrap.sh | bash

  # Local clone:
  ./rantaiclaw_install.sh

  # Install + onboard right away:
  ./rantaiclaw_install.sh --interactive-onboard

  # Build from source (contributor / unsupported platform):
  ./rantaiclaw_install.sh --from-source --install-system-deps --install-rust

  # Containerized:
  ./rantaiclaw_install.sh --docker --interactive-onboard

Environment:
  RANTAICLAW_CONTAINER_CLI     Container CLI command (default: docker; auto-fallback: podman)
  RANTAICLAW_DOCKER_DATA_DIR   Host path for Docker config/workspace persistence
  RANTAICLAW_DOCKER_IMAGE      Docker image tag to build/run (default: rantaiclaw-bootstrap:local)
  RANTAICLAW_RELEASE_BASE_URL  Override release-archive base URL (mirror / staging)
  RANTAICLAW_REPO_URL          Override git URL for source/docker mode clones
  RANTAICLAW_FALLBACK_IMAGE    Override fallback Docker image
  RANTAICLAW_INSTALL_DIR       Override install directory (default: ~/.cargo/bin or ~/.local/bin)
  RANTAICLAW_API_KEY           Used when --api-key is not provided
  RANTAICLAW_PROVIDER          Used when --provider is not provided (default: openrouter)
  RANTAICLAW_MODEL             Used when --model is not provided
  RANTAICLAW_BOOTSTRAP_MIN_RAM_MB   Minimum RAM threshold for source build preflight (default: 2048)
  RANTAICLAW_BOOTSTRAP_MIN_DISK_MB  Minimum free disk threshold for source build preflight (default: 6144)
  VERIFY_CHECKSUM              Set to "false" to skip SHA256 check (same as --no-verify-checksum)
  RANTAICLAW_DISABLE_ALPINE_AUTO_DEPS
                              Set to 1 to disable Alpine auto-install of missing prerequisites
USAGE
}

have_cmd() {
  command -v "$1" >/dev/null 2>&1
}

get_total_memory_mb() {
  case "$(uname -s)" in
    Linux)
      if [[ -r /proc/meminfo ]]; then
        awk '/MemTotal:/ {printf "%d\n", $2 / 1024}' /proc/meminfo
      fi
      ;;
    Darwin)
      if have_cmd sysctl; then
        local bytes
        bytes="$(sysctl -n hw.memsize 2>/dev/null || true)"
        if [[ "$bytes" =~ ^[0-9]+$ ]]; then
          echo $((bytes / 1024 / 1024))
        fi
      fi
      ;;
  esac
}

get_available_disk_mb() {
  local path="${1:-.}"
  local free_kb
  free_kb="$(df -Pk "$path" 2>/dev/null | awk 'NR==2 {print $4}')"
  if [[ "$free_kb" =~ ^[0-9]+$ ]]; then
    echo $((free_kb / 1024))
  fi
}

detect_release_target() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os:$arch" in
    Linux:x86_64)
      echo "x86_64-unknown-linux-gnu"
      ;;
    Linux:aarch64|Linux:arm64)
      echo "aarch64-unknown-linux-gnu"
      ;;
    Linux:armv7l|Linux:armv6l)
      echo "armv7-unknown-linux-gnueabihf"
      ;;
    Darwin:x86_64)
      echo "x86_64-apple-darwin"
      ;;
    Darwin:arm64|Darwin:aarch64)
      echo "aarch64-apple-darwin"
      ;;
    *)
      return 1
      ;;
  esac
}

should_attempt_prebuilt_for_resources() {
  local workspace="${1:-.}"
  local min_ram_mb min_disk_mb total_ram_mb free_disk_mb low_resource

  min_ram_mb="${RANTAICLAW_BOOTSTRAP_MIN_RAM_MB:-2048}"
  min_disk_mb="${RANTAICLAW_BOOTSTRAP_MIN_DISK_MB:-6144}"
  total_ram_mb="$(get_total_memory_mb || true)"
  free_disk_mb="$(get_available_disk_mb "$workspace" || true)"
  low_resource=false

  if [[ "$total_ram_mb" =~ ^[0-9]+$ && "$total_ram_mb" -lt "$min_ram_mb" ]]; then
    low_resource=true
  fi
  if [[ "$free_disk_mb" =~ ^[0-9]+$ && "$free_disk_mb" -lt "$min_disk_mb" ]]; then
    low_resource=true
  fi

  if [[ "$low_resource" == true ]]; then
    warn "Source build preflight indicates constrained resources."
    if [[ "$total_ram_mb" =~ ^[0-9]+$ ]]; then
      warn "Detected RAM: ${total_ram_mb}MB (recommended >= ${min_ram_mb}MB for local source builds)."
    else
      warn "Unable to detect total RAM automatically."
    fi
    if [[ "$free_disk_mb" =~ ^[0-9]+$ ]]; then
      warn "Detected free disk: ${free_disk_mb}MB (recommended >= ${min_disk_mb}MB)."
    else
      warn "Unable to detect free disk space automatically."
    fi
    return 0
  fi

  return 1
}

sha256_compute() {
  if have_cmd sha256sum; then
    sha256sum "$1" | awk '{print $1}'
  elif have_cmd shasum; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    return 1
  fi
}

install_prebuilt_binary() {
  local target archive_url checksums_url temp_dir archive_path checksums_path
  local extracted_bin install_dir expected_sum actual_sum
  local archive_basename

  if ! have_cmd curl; then
    warn "curl is required for pre-built binary installation."
    return 1
  fi
  if ! have_cmd tar; then
    warn "tar is required for pre-built binary installation."
    return 1
  fi

  target="$(detect_release_target || true)"
  if [[ -z "$target" ]]; then
    warn "No pre-built binary available for $(uname -s)/$(uname -m)."
    info "Supported targets: x86_64/aarch64/armv7 Linux, x86_64/arm64 macOS, x86_64 Windows."
    info "Re-run with --from-source to build for your platform."
    return 1
  fi

  archive_url="${RANTAICLAW_RELEASE_BASE_URL:-https://github.com/RantAI-dev/RantAIClaw/releases/latest/download}/rantaiclaw-${target}.tar.gz"
  checksums_url="${RANTAICLAW_RELEASE_BASE_URL:-https://github.com/RantAI-dev/RantAIClaw/releases/latest/download}/SHA256SUMS"
  temp_dir="$(mktemp -d -t rantaiclaw-prebuilt-XXXXXX)"
  archive_path="$temp_dir/rantaiclaw-${target}.tar.gz"
  checksums_path="$temp_dir/SHA256SUMS"

  next_step "Installing pre-built binary for $target"
  info "Source: $archive_url"
  spinner_start "Downloading rantaiclaw-${target}.tar.gz"
  if curl -fsSL "$archive_url" -o "$archive_path"; then
    spinner_stop "Download complete"
  else
    spinner_stop_fail "Download failed"
    warn "Could not download release asset: $archive_url"
    warn "Check network connectivity, or re-run with --from-source to build locally."
    rm -rf "$temp_dir"
    return 1
  fi

  archive_basename="rantaiclaw-${target}.tar.gz"
  if [[ "${VERIFY_CHECKSUM:-true}" == "true" ]]; then
    spinner_start "Verifying SHA256 checksum"
    if curl -fsSL "$checksums_url" -o "$checksums_path"; then
      expected_sum="$(awk -v name="$archive_basename" '$2==name || $2=="./"name {print $1; exit}' "$checksums_path" 2>/dev/null || true)"
      if [[ -z "$expected_sum" ]]; then
        spinner_stop_fail "Checksum file missing entry for $archive_basename"
        warn "Could not find $archive_basename in SHA256SUMS — release artifacts may be mid-publish."
        warn "Re-run with VERIFY_CHECKSUM=false to skip (offline / mirror scenarios)."
        rm -rf "$temp_dir"
        return 1
      fi
      actual_sum="$(sha256_compute "$archive_path" || true)"
      if [[ -z "$actual_sum" ]]; then
        spinner_stop_fail "No sha256 utility available (sha256sum/shasum)"
        warn "Install coreutils (Linux) or perl/shasum (macOS), or set VERIFY_CHECKSUM=false to skip."
        rm -rf "$temp_dir"
        return 1
      fi
      if [[ "$expected_sum" != "$actual_sum" ]]; then
        spinner_stop_fail "Checksum mismatch"
        error "Expected: $expected_sum"
        error "Actual:   $actual_sum"
        error "Refusing to install a tampered or corrupt archive."
        rm -rf "$temp_dir"
        return 1
      fi
      spinner_stop "Checksum verified"
    else
      spinner_stop_fail "Could not fetch SHA256SUMS"
      warn "Re-run with VERIFY_CHECKSUM=false to skip verification (offline / mirror scenarios)."
      rm -rf "$temp_dir"
      return 1
    fi
  else
    info "Skipping checksum verification (VERIFY_CHECKSUM=false)"
  fi

  if ! tar -xzf "$archive_path" -C "$temp_dir"; then
    warn "Failed to extract pre-built archive."
    rm -rf "$temp_dir"
    return 1
  fi

  extracted_bin="$temp_dir/rantaiclaw"
  if [[ ! -x "$extracted_bin" ]]; then
    extracted_bin="$(find "$temp_dir" -maxdepth 2 -type f -name rantaiclaw -perm -u+x | head -n 1 || true)"
  fi
  if [[ -z "$extracted_bin" || ! -x "$extracted_bin" ]]; then
    warn "Archive did not contain an executable rantaiclaw binary."
    rm -rf "$temp_dir"
    return 1
  fi

  install_dir="$(resolve_install_dir)"
  mkdir -p "$install_dir"
  install -m 0755 "$extracted_bin" "$install_dir/rantaiclaw"
  rm -rf "$temp_dir"

  success "Installed rantaiclaw to $install_dir/rantaiclaw"
  if [[ -x "$install_dir/rantaiclaw" ]]; then
    local version_line
    version_line="$("$install_dir/rantaiclaw" --version 2>/dev/null | head -n 1 || true)"
    [[ -n "$version_line" ]] && info "Version: $version_line"
  fi
  print_path_hint "$install_dir"

  return 0
}

# Decide where the binary should land. Prefer ~/.cargo/bin when cargo exists
# (matches `cargo install`); otherwise default to ~/.local/bin which is in
# PATH on most modern distros (Ubuntu, Fedora, Arch via systemd's user dirs).
# Override with RANTAICLAW_INSTALL_DIR.
resolve_install_dir() {
  if [[ -n "${RANTAICLAW_INSTALL_DIR:-}" ]]; then
    printf '%s' "$RANTAICLAW_INSTALL_DIR"
    return
  fi
  if have_cmd cargo || [[ -d "$HOME/.cargo/bin" ]]; then
    printf '%s' "$HOME/.cargo/bin"
    return
  fi
  printf '%s' "$HOME/.local/bin"
}

print_path_hint() {
  local dir="$1"
  local shell_name shell_rc
  if [[ ":$PATH:" == *":$dir:"* ]]; then
    return 0
  fi
  warn "$dir is not in your PATH for this shell."
  shell_name="$(basename "${SHELL:-/bin/bash}")"
  case "$shell_name" in
    zsh)  shell_rc="~/.zshrc" ;;
    fish) shell_rc="~/.config/fish/config.fish" ;;
    *)    shell_rc="~/.bashrc" ;;
  esac
  echo "    Add this line to $shell_rc:"
  if [[ "$shell_name" == "fish" ]]; then
    echo "        fish_add_path $dir"
  else
    echo "        export PATH=\"$dir:\$PATH\""
  fi
  echo "    Or run it now in this shell:"
  if [[ "$shell_name" == "fish" ]]; then
    echo "        fish_add_path $dir"
  else
    echo "        export PATH=\"$dir:\$PATH\""
  fi
}

run_privileged() {
  if [[ "$(id -u)" -eq 0 ]]; then
    "$@"
  elif have_cmd sudo; then
    sudo "$@"
  else
    error "sudo is required to install system dependencies."
    return 1
  fi
}

is_container_runtime() {
  if [[ -f /.dockerenv || -f /run/.containerenv ]]; then
    return 0
  fi

  if [[ -r /proc/1/cgroup ]] && grep -Eq '(docker|containerd|kubepods|podman|lxc)' /proc/1/cgroup; then
    return 0
  fi

  return 1
}

run_pacman() {
  if ! have_cmd pacman; then
    error "pacman is not available."
    return 1
  fi

  if ! is_container_runtime; then
    run_privileged pacman "$@"
    return $?
  fi

  local pacman_cfg_tmp=""
  local pacman_rc=0
  pacman_cfg_tmp="$(mktemp /tmp/rantaiclaw-pacman.XXXXXX.conf)"
  cp /etc/pacman.conf "$pacman_cfg_tmp"
  if ! grep -Eq '^[[:space:]]*DisableSandboxSyscalls([[:space:]]|$)' "$pacman_cfg_tmp"; then
    printf '\nDisableSandboxSyscalls\n' >> "$pacman_cfg_tmp"
  fi

  if run_privileged pacman --config "$pacman_cfg_tmp" "$@"; then
    pacman_rc=0
  else
    pacman_rc=$?
  fi

  rm -f "$pacman_cfg_tmp"
  return "$pacman_rc"
}

ALPINE_PREREQ_PACKAGES=(
  bash
  build-base
  pkgconf
  git
  curl
  openssl-dev
  perl
  ca-certificates
)
ALPINE_MISSING_PKGS=()

find_missing_alpine_prereqs() {
  ALPINE_MISSING_PKGS=()
  if ! have_cmd apk; then
    return 0
  fi

  local pkg=""
  for pkg in "${ALPINE_PREREQ_PACKAGES[@]}"; do
    if ! apk info -e "$pkg" >/dev/null 2>&1; then
      ALPINE_MISSING_PKGS+=("$pkg")
    fi
  done
}

bool_to_word() {
  if [[ "$1" == true ]]; then
    echo "yes"
  else
    echo "no"
  fi
}

ask_yes_no() {
  local question="$1"
  local default_answer="${2:-yes}"
  local prompt=""
  local answer=""

  if [[ "$default_answer" == "yes" ]]; then
    prompt="[Y/n]"
  else
    prompt="[y/N]"
  fi

  while true; do
    if [[ "$IS_INTERACTIVE" == "true" ]]; then
      read -r -p "$question $prompt " answer || answer=""
    elif [[ -r /dev/tty && -w /dev/tty ]]; then
      printf '%s %s ' "$question" "$prompt" > /dev/tty
      IFS= read -r answer < /dev/tty || answer=""
    else
      answer=""
    fi
    answer="${answer:-$default_answer}"
    case "$(printf '%s' "$answer" | tr '[:upper:]' '[:lower:]')" in
      y|yes)
        return 0
        ;;
      n|no)
        return 1
        ;;
      *)
        echo "Please answer yes or no."
        ;;
    esac
  done
}

install_system_deps() {
  next_step "Installing system dependencies"

  case "$(uname -s)" in
    Linux)
      if have_cmd apk; then
        find_missing_alpine_prereqs
        if [[ ${#ALPINE_MISSING_PKGS[@]} -eq 0 ]]; then
          info "Alpine prerequisites already installed"
        else
          info "Installing Alpine prerequisites: ${ALPINE_MISSING_PKGS[*]}"
          run_privileged apk add --no-cache "${ALPINE_MISSING_PKGS[@]}"
        fi
      elif have_cmd apt-get; then
        run_privileged apt-get update -qq
        run_privileged apt-get install -y build-essential pkg-config git curl
      elif have_cmd dnf; then
        run_privileged dnf install -y \
          gcc \
          gcc-c++ \
          make \
          pkgconf-pkg-config \
          git \
          curl \
          openssl-devel \
          perl
      elif have_cmd pacman; then
        run_pacman -Sy --noconfirm
        run_pacman -S --noconfirm --needed \
          gcc \
          make \
          pkgconf \
          git \
          curl \
          openssl \
          perl \
          ca-certificates
      else
        warn "Unsupported Linux distribution. Install compiler toolchain + pkg-config + git + curl + OpenSSL headers + perl manually."
      fi
      ;;
    Darwin)
      if ! xcode-select -p >/dev/null 2>&1; then
        info "Installing Xcode Command Line Tools"
        xcode-select --install || true
        cat <<'MSG'
Please complete the Xcode Command Line Tools installation dialog,
then re-run bootstrap.
MSG
        exit 0
      fi
      if ! have_cmd git; then
        warn "git is not available. Install git (e.g., Homebrew) and re-run bootstrap."
      fi
      ;;
    *)
      warn "Unsupported OS for automatic dependency install. Continuing without changes."
      ;;
  esac
}

install_rust_toolchain() {
  if have_cmd cargo && have_cmd rustc; then
    info "Rust already installed: $(rustc --version)"
    return
  fi

  if ! have_cmd curl; then
    error "curl is required to install Rust via rustup."
    exit 1
  fi

  next_step "Installing Rust via rustup"
  spinner_start "Installing Rust toolchain via rustup"
  if curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y; then
    spinner_stop "Rust toolchain installed"
  else
    spinner_stop_fail "Rust toolchain installation failed"
    error "rustup installer exited non-zero."
    exit 1
  fi

  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
  fi

  if ! have_cmd cargo; then
    error "Rust installation completed but cargo is still unavailable in PATH."
    error "Run: source \"$HOME/.cargo/env\""
    exit 1
  fi
}

run_guided_installer() {
  local os_name="$1"
  local provider_input=""
  local model_input=""
  local api_key_input=""
  local detected_target

  echo
  echo "RantaiClaw guided installer"
  echo "A few quick questions, then we'll get you running."
  echo

  detected_target="$(detect_release_target || true)"

  if [[ -n "$detected_target" ]]; then
    info "Detected platform: $detected_target"
    if ask_yes_no "Install the latest pre-built binary (recommended)?" "yes"; then
      FORCE_SOURCE_BUILD=false
      PREFER_PREBUILT=true
    else
      FORCE_SOURCE_BUILD=true
      PREFER_PREBUILT=false
    fi
  else
    warn "No pre-built binary is published for $(uname -s)/$(uname -m)."
    info "Falling back to source build."
    FORCE_SOURCE_BUILD=true
    PREFER_PREBUILT=false
  fi

  if [[ "$FORCE_SOURCE_BUILD" == true ]]; then
    if [[ "$os_name" == "Linux" ]]; then
      if ask_yes_no "Install Linux build dependencies (toolchain/pkg-config/git/curl)?" "yes"; then
        INSTALL_SYSTEM_DEPS=true
      fi
    else
      if ask_yes_no "Install system dependencies for $os_name?" "no"; then
        INSTALL_SYSTEM_DEPS=true
      fi
    fi

    if have_cmd cargo && have_cmd rustc; then
      info "Detected Rust toolchain: $(rustc --version)"
    else
      if ask_yes_no "Rust toolchain not found. Install Rust via rustup now?" "yes"; then
        INSTALL_RUST=true
      fi
    fi
  fi

  if ask_yes_no "Run onboarding (connect a provider) after install?" "yes"; then
    RUN_ONBOARD=true
    if ask_yes_no "Use interactive onboarding?" "yes"; then
      INTERACTIVE_ONBOARD=true
    else
      INTERACTIVE_ONBOARD=false
      provider_input="$(prompt_input "Provider" "$PROVIDER")"
      if [[ -n "$provider_input" ]]; then
        PROVIDER="$provider_input"
      fi

      model_input="$(prompt_input "Model" "${MODEL:-}")"
      if [[ -n "$model_input" ]]; then
        MODEL="$model_input"
      fi

      if [[ -z "$API_KEY" ]]; then
        api_key_input="$(prompt_input_secret "API key (hidden, leave empty to switch to interactive onboarding)")"
        if [[ -n "$api_key_input" ]]; then
          API_KEY="$api_key_input"
        else
          warn "No API key entered. Using interactive onboarding instead."
          INTERACTIVE_ONBOARD=true
        fi
      fi
    fi
  fi

  echo
  info "Install plan"
  if [[ "$FORCE_SOURCE_BUILD" == false ]]; then
    echo "    install-mode: pre-built binary ($detected_target)"
  else
    echo "    install-mode: build from source"
    echo "    install-system-deps: $(bool_to_word "$INSTALL_SYSTEM_DEPS")"
    echo "    install-rust: $(bool_to_word "$INSTALL_RUST")"
  fi
  echo "    onboard: $(bool_to_word "$RUN_ONBOARD")"
  if [[ "$RUN_ONBOARD" == true ]]; then
    echo "    interactive-onboard: $(bool_to_word "$INTERACTIVE_ONBOARD")"
    if [[ "$INTERACTIVE_ONBOARD" == false ]]; then
      echo "    provider: $PROVIDER"
      if [[ -n "$MODEL" ]]; then
        echo "    model: $MODEL"
      fi
    fi
  fi

  echo
  if ! ask_yes_no "Proceed?" "yes"; then
    info "Installation canceled by user."
    exit 0
  fi
}

resolve_container_cli() {
  local requested_cli
  requested_cli="${RANTAICLAW_CONTAINER_CLI:-docker}"

  if have_cmd "$requested_cli"; then
    CONTAINER_CLI="$requested_cli"
    return 0
  fi

  if [[ "$requested_cli" == "docker" ]] && have_cmd podman; then
    warn "docker CLI not found; falling back to podman."
    CONTAINER_CLI="podman"
    return 0
  fi

  error "Container CLI '$requested_cli' is not installed."
  if [[ "$requested_cli" != "docker" ]]; then
    error "Set RANTAICLAW_CONTAINER_CLI to an installed Docker-compatible CLI (e.g., docker or podman)."
  else
    error "Install Docker, install podman, or set RANTAICLAW_CONTAINER_CLI to an available Docker-compatible CLI."
  fi
  exit 1
}

ensure_docker_ready() {
  resolve_container_cli

  if ! "$CONTAINER_CLI" info >/dev/null 2>&1; then
    error "Container runtime is not reachable via '$CONTAINER_CLI'."
    error "Start the container runtime and re-run bootstrap."
    exit 1
  fi
}

run_docker_bootstrap() {
  local docker_image docker_data_dir default_data_dir fallback_image
  local config_mount workspace_mount
  local -a container_run_user_args container_run_namespace_args
  docker_image="${RANTAICLAW_DOCKER_IMAGE:-rantaiclaw-bootstrap:local}"
  fallback_image="${RANTAICLAW_FALLBACK_IMAGE:-ghcr.io/rantai-dev/rantaiclaw:latest}"
  if [[ "$TEMP_CLONE" == true ]]; then
    default_data_dir="$HOME/.rantaiclaw-docker"
  else
    default_data_dir="$WORK_DIR/.rantaiclaw-docker"
  fi
  docker_data_dir="${RANTAICLAW_DOCKER_DATA_DIR:-$default_data_dir}"
  DOCKER_DATA_DIR="$docker_data_dir"

  mkdir -p "$docker_data_dir/.rantaiclaw" "$docker_data_dir/workspace"

  if [[ "$SKIP_INSTALL" == true ]]; then
    warn "--skip-install has no effect with --docker."
  fi

  if [[ "$SKIP_BUILD" == false ]]; then
    info "Building Docker image ($docker_image)"
    "$CONTAINER_CLI" build --target release -t "$docker_image" "$WORK_DIR"
  else
    info "Skipping Docker image build"
    if ! "$CONTAINER_CLI" image inspect "$docker_image" >/dev/null 2>&1; then
      warn "Local Docker image ($docker_image) was not found."
      info "Pulling official RantaiClaw image ($fallback_image)"
      if ! "$CONTAINER_CLI" pull "$fallback_image"; then
        error "Failed to pull fallback Docker image: $fallback_image"
        error "Run without --skip-build to build locally, or verify access to GHCR."
        exit 1
      fi
      if [[ "$docker_image" != "$fallback_image" ]]; then
        info "Tagging fallback image as $docker_image"
        "$CONTAINER_CLI" tag "$fallback_image" "$docker_image"
      fi
    fi
  fi

  config_mount="$docker_data_dir/.rantaiclaw:/rantaiclaw-data/.rantaiclaw"
  workspace_mount="$docker_data_dir/workspace:/rantaiclaw-data/workspace"
  if [[ "$CONTAINER_CLI" == "podman" ]]; then
    config_mount+=":Z"
    workspace_mount+=":Z"
    container_run_namespace_args=(--userns keep-id)
    container_run_user_args=(--user "$(id -u):$(id -g)")
  else
    container_run_namespace_args=()
    container_run_user_args=(--user "$(id -u):$(id -g)")
  fi

  info "Docker data directory: $docker_data_dir"
  info "Container CLI: $CONTAINER_CLI"

  local onboard_cmd=()
  if [[ "$INTERACTIVE_ONBOARD" == true ]]; then
    info "Launching interactive onboarding in container"
    onboard_cmd=(onboard --interactive)
  else
    if [[ -z "$API_KEY" ]]; then
      cat <<'MSG'
==> Onboarding requested, but API key not provided.
Use either:
  --api-key "sk-..."
or:
  RANTAICLAW_API_KEY="sk-..." ./rantaiclaw_install.sh --docker
or run interactive:
  ./rantaiclaw_install.sh --docker --interactive-onboard
MSG
      exit 1
    fi
    if [[ -n "$MODEL" ]]; then
      info "Launching quick onboarding in container (provider: $PROVIDER, model: $MODEL)"
    else
      info "Launching quick onboarding in container (provider: $PROVIDER)"
    fi
    onboard_cmd=(onboard --api-key "$API_KEY" --provider "$PROVIDER")
    if [[ -n "$MODEL" ]]; then
      onboard_cmd+=(--model "$MODEL")
    fi
  fi

  "$CONTAINER_CLI" run --rm -it \
    "${container_run_namespace_args[@]}" \
    "${container_run_user_args[@]}" \
    -e HOME=/rantaiclaw-data \
    -e RANTAICLAW_WORKSPACE=/rantaiclaw-data/workspace \
    -v "$config_mount" \
    -v "$workspace_mount" \
    "$docker_image" \
    "${onboard_cmd[@]}"
}

SCRIPT_PATH="${BASH_SOURCE[0]:-$0}"
SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_PATH")" >/dev/null 2>&1 && pwd || pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." >/dev/null 2>&1 && pwd || pwd)"
REPO_URL="${RANTAICLAW_REPO_URL:-https://github.com/RantAI-dev/RantAIClaw.git}"
ORIGINAL_ARG_COUNT=$#
GUIDED_MODE="auto"

DOCKER_MODE=false
INSTALL_SYSTEM_DEPS=false
INSTALL_RUST=false
# Binary-first is the default. PREFER_PREBUILT stays true unless the user
# explicitly opts into source build via --from-source / --force-source-build.
PREFER_PREBUILT=true
PREBUILT_ONLY=false
FORCE_SOURCE_BUILD=false
VERIFY_CHECKSUM="${VERIFY_CHECKSUM:-true}"
RUN_ONBOARD=false
INTERACTIVE_ONBOARD=false
SKIP_BUILD=false
SKIP_INSTALL=false
PREBUILT_INSTALLED=false
CONTAINER_CLI="${RANTAICLAW_CONTAINER_CLI:-docker}"
API_KEY="${RANTAICLAW_API_KEY:-}"
PROVIDER="${RANTAICLAW_PROVIDER:-openrouter}"
MODEL="${RANTAICLAW_MODEL:-}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --guided)
      GUIDED_MODE="on"
      shift
      ;;
    --no-guided)
      GUIDED_MODE="off"
      shift
      ;;
    --docker)
      DOCKER_MODE=true
      shift
      ;;
    --install-system-deps)
      INSTALL_SYSTEM_DEPS=true
      shift
      ;;
    --install-rust)
      INSTALL_RUST=true
      shift
      ;;
    --prefer-prebuilt)
      # No-op: binary is the default since installer-ux upgrade. Kept for
      # backward compatibility with old README/docs and CI invocations.
      PREFER_PREBUILT=true
      shift
      ;;
    --prebuilt-only)
      PREBUILT_ONLY=true
      PREFER_PREBUILT=true
      shift
      ;;
    --from-source|--force-source-build)
      FORCE_SOURCE_BUILD=true
      PREFER_PREBUILT=false
      PREBUILT_ONLY=false
      shift
      ;;
    --no-verify-checksum)
      VERIFY_CHECKSUM=false
      shift
      ;;
    --onboard)
      RUN_ONBOARD=true
      shift
      ;;
    --interactive-onboard)
      RUN_ONBOARD=true
      INTERACTIVE_ONBOARD=true
      shift
      ;;
    --api-key)
      API_KEY="${2:-}"
      [[ -n "$API_KEY" ]] || {
        error "--api-key requires a value"
        exit 1
      }
      shift 2
      ;;
    --provider)
      PROVIDER="${2:-}"
      [[ -n "$PROVIDER" ]] || {
        error "--provider requires a value"
        exit 1
      }
      shift 2
      ;;
    --model)
      MODEL="${2:-}"
      [[ -n "$MODEL" ]] || {
        error "--model requires a value"
        exit 1
      }
      shift 2
      ;;
    --build-first)
      SKIP_BUILD=false
      shift
      ;;
    --skip-build)
      SKIP_BUILD=true
      shift
      ;;
    --skip-install)
      SKIP_INSTALL=true
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      error "unknown option: $1"
      echo
      usage
      exit 1
      ;;
  esac
done

# Compute total visible steps from CLI flag combination so [N/T] labels
# show a stable progress denominator across the whole install. The shape
# follows the user's chosen mode: binary path = 1 install step, source
# path = 2 (build + install). Optional steps (system deps, Rust install,
# onboarding) add on top.
compute_step_total() {
  local total=1  # final bootstrap label
  [[ "${INSTALL_SYSTEM_DEPS:-false}" == "true" ]] && total=$((total + 1))
  [[ "${INSTALL_RUST:-false}" == "true" ]] && total=$((total + 1))
  if [[ "${FORCE_SOURCE_BUILD:-false}" == "true" ]]; then
    [[ "${SKIP_BUILD:-false}" != "true" ]] && total=$((total + 1))     # source build
    [[ "${SKIP_INSTALL:-false}" != "true" ]] && total=$((total + 1))   # cargo install
  else
    total=$((total + 1))   # prebuilt download+install (covers both phases)
  fi
  [[ "${RUN_ONBOARD:-false}" == "true" ]] && total=$((total + 1))
  __STEP_TOTAL="$total"
  __STEP_CURRENT=0
}

# Increment step counter and print a step label.
next_step() {
  __STEP_CURRENT=$((__STEP_CURRENT + 1))
  step "$__STEP_CURRENT/$__STEP_TOTAL" "$1"
}

compute_step_total

# Opening banner — sets the visual identity for the install run.
print_banner

OS_NAME="$(uname -s)"
if [[ "$GUIDED_MODE" == "auto" ]]; then
  if [[ "$OS_NAME" == "Linux" && "$ORIGINAL_ARG_COUNT" -eq 0 && -t 0 && -t 1 ]]; then
    GUIDED_MODE="on"
  else
    GUIDED_MODE="off"
  fi
fi

if [[ "$DOCKER_MODE" == true && "$GUIDED_MODE" == "on" ]]; then
  warn "--guided is ignored with --docker."
  GUIDED_MODE="off"
fi

if [[ "$GUIDED_MODE" == "on" ]]; then
  run_guided_installer "$OS_NAME"
fi

if [[ "$DOCKER_MODE" == true ]]; then
  if [[ "$INSTALL_SYSTEM_DEPS" == true ]]; then
    warn "--install-system-deps is ignored with --docker."
  fi
  if [[ "$INSTALL_RUST" == true ]]; then
      warn "--install-rust is ignored with --docker."
  fi
else
  if [[ "$OS_NAME" == "Linux" && -z "${RANTAICLAW_DISABLE_ALPINE_AUTO_DEPS:-}" ]] && have_cmd apk; then
    find_missing_alpine_prereqs
    if [[ ${#ALPINE_MISSING_PKGS[@]} -gt 0 && "$INSTALL_SYSTEM_DEPS" == false ]]; then
      info "Detected Alpine with missing prerequisites: ${ALPINE_MISSING_PKGS[*]}"
      info "Auto-enabling system dependency installation (set RANTAICLAW_DISABLE_ALPINE_AUTO_DEPS=1 to disable)."
      INSTALL_SYSTEM_DEPS=true
    fi
  fi

  if [[ "$INSTALL_SYSTEM_DEPS" == true ]]; then
    install_system_deps
  fi

  if [[ "$INSTALL_RUST" == true ]]; then
    install_rust_toolchain
  fi
fi

WORK_DIR="$ROOT_DIR"
TEMP_CLONE=false
TEMP_DIR=""

cleanup() {
  __ui_spinner_kill 2>/dev/null || true
  if [[ "$TEMP_CLONE" == true && -n "$TEMP_DIR" && -d "$TEMP_DIR" ]]; then
    rm -rf "$TEMP_DIR"
  fi
}
trap cleanup EXIT

# Clone the repo into a temp dir on demand. Used only when source build or
# docker mode actually needs files on disk — never on the binary-install path.
# $1: human-readable reason (printed in the error if git is missing)
ensure_temp_clone() {
  local reason="$1"
  if ! have_cmd git; then
    error "git is required: $reason."
    error "Install git, or use the default binary install (omit --from-source / --docker)."
    exit 1
  fi
  TEMP_DIR="$(mktemp -d -t rantaiclaw-bootstrap-XXXXXX)"
  info "Cloning $REPO_URL (depth 1) for $reason"
  git clone --depth 1 "$REPO_URL" "$TEMP_DIR"
  WORK_DIR="$TEMP_DIR"
  TEMP_CLONE=true
  HAS_LOCAL_REPO=true
}

# Decide whether we already have a local repo checkout (script run from a
# clone) or are running detached (curl | bash). When detached, we only need
# the repo on disk for source build / docker mode; binary install can run
# entirely from release artifacts.
HAS_LOCAL_REPO=false
if [[ -f "$WORK_DIR/Cargo.toml" ]]; then
  HAS_LOCAL_REPO=true
elif [[ -f "$(pwd)/Cargo.toml" ]]; then
  WORK_DIR="$(pwd)"
  HAS_LOCAL_REPO=true
fi

next_step "RantaiClaw bootstrap"
if [[ "$HAS_LOCAL_REPO" == true ]]; then
  echo "    workspace: $WORK_DIR"
else
  echo "    workspace: (none — binary install from release artifacts)"
fi

if [[ "$DOCKER_MODE" == true ]]; then
  if [[ "$HAS_LOCAL_REPO" == false ]]; then
    ensure_temp_clone "Docker mode needs a repository checkout to build the local image"
  fi
  cd "$WORK_DIR"
  ensure_docker_ready
  if [[ "$RUN_ONBOARD" == false ]]; then
    RUN_ONBOARD=true
    if [[ -z "$API_KEY" ]]; then
      INTERACTIVE_ONBOARD=true
    fi
  fi
  run_docker_bootstrap
  print_success_banner \
    "./rantaiclaw_install.sh --docker --interactive-onboard   — run guided onboarding" \
    "./rantaiclaw_install.sh --docker --api-key \"sk-...\" --provider openrouter   — onboard non-interactively"
  info "Containerized RantaiClaw data persisted under: $DOCKER_DATA_DIR"
  exit 0
fi

# Try the binary install path first (the new default). This runs without
# any clone — curl|bash to install never triggers a GitHub auth prompt.
if [[ "$FORCE_SOURCE_BUILD" == false && "$PREFER_PREBUILT" == true ]]; then
  if install_prebuilt_binary; then
    PREBUILT_INSTALLED=true
    SKIP_BUILD=true
    SKIP_INSTALL=true
  elif [[ "$PREBUILT_ONLY" == true ]]; then
    error "No pre-built binary available for this platform."
    error "Re-run with --from-source on a machine with a Rust toolchain to build locally."
    exit 1
  else
    warn "Binary install unavailable; falling back to source build."
    FORCE_SOURCE_BUILD=true
  fi
fi

# Source build needs the repository on disk. Clone on demand only when
# the binary path didn't run or didn't succeed.
if [[ "$PREBUILT_INSTALLED" == false && "$HAS_LOCAL_REPO" == false ]]; then
  ensure_temp_clone "Source build needs a repository checkout"
fi

cd "$WORK_DIR"

if [[ "$PREBUILT_INSTALLED" == false && ( "$SKIP_BUILD" == false || "$SKIP_INSTALL" == false ) ]] && ! have_cmd cargo; then
  error "cargo is not installed."
  cat <<'MSG' >&2
Install Rust first: https://rustup.rs/
or re-run with:
  ./rantaiclaw_install.sh --install-rust
MSG
  exit 1
fi

if [[ "$SKIP_BUILD" == false ]]; then
  next_step "Building release binary"
  spinner_start "Building rantaiclaw (cargo build --release)"
  if cargo build --release --locked; then
    spinner_stop "Build complete"
  else
    spinner_stop_fail "Build failed"
    error "cargo build failed; see output above for details."
    exit 1
  fi
else
  info "Skipping build"
fi

if [[ "$SKIP_INSTALL" == false ]]; then
  next_step "Installing rantaiclaw to cargo bin"
  spinner_start "Installing rantaiclaw to cargo bin"
  if cargo install --path "$WORK_DIR" --force --locked; then
    spinner_stop "Installed rantaiclaw"
  else
    spinner_stop_fail "cargo install failed"
    error "cargo install failed; see output above for details."
    exit 1
  fi
else
  info "Skipping install"
fi

RANTAICLAW_BIN=""
if have_cmd rantaiclaw; then
  RANTAICLAW_BIN="rantaiclaw"
elif [[ -x "$HOME/.cargo/bin/rantaiclaw" ]]; then
  RANTAICLAW_BIN="$HOME/.cargo/bin/rantaiclaw"
elif [[ -x "$HOME/.local/bin/rantaiclaw" ]]; then
  RANTAICLAW_BIN="$HOME/.local/bin/rantaiclaw"
elif [[ -n "${RANTAICLAW_INSTALL_DIR:-}" && -x "${RANTAICLAW_INSTALL_DIR}/rantaiclaw" ]]; then
  RANTAICLAW_BIN="${RANTAICLAW_INSTALL_DIR}/rantaiclaw"
elif [[ -x "$WORK_DIR/target/release/rantaiclaw" ]]; then
  RANTAICLAW_BIN="$WORK_DIR/target/release/rantaiclaw"
fi

if [[ "$RUN_ONBOARD" == true ]]; then
  if [[ -z "$RANTAICLAW_BIN" ]]; then
    error "onboarding requested but rantaiclaw binary is not available."
    error "Run without --skip-install, or ensure rantaiclaw is in PATH."
    exit 1
  fi

  if [[ "$INTERACTIVE_ONBOARD" == true ]]; then
    next_step "Running interactive onboarding"
    "$RANTAICLAW_BIN" onboard --interactive
  else
    if [[ -z "$API_KEY" ]]; then
      cat <<'MSG'
==> Onboarding requested, but API key not provided.
Use either:
  --api-key "sk-..."
or:
  RANTAICLAW_API_KEY="sk-..." ./rantaiclaw_install.sh --onboard
or run interactive:
  ./rantaiclaw_install.sh --interactive-onboard
MSG
      exit 1
    fi
    if [[ -n "$MODEL" ]]; then
      next_step "Running quick onboarding (provider: $PROVIDER, model: $MODEL)"
    else
      next_step "Running quick onboarding (provider: $PROVIDER)"
    fi
    ONBOARD_CMD=("$RANTAICLAW_BIN" onboard --api-key "$API_KEY" --provider "$PROVIDER")
    if [[ -n "$MODEL" ]]; then
      ONBOARD_CMD+=(--model "$MODEL")
    fi
    "${ONBOARD_CMD[@]}"
  fi
fi

# Show success banner only when real install work was performed.
# Pure --skip-build --skip-install runs (no prebuilt either) get a plain notice.
if [[ "$SKIP_BUILD" == "true" && "$SKIP_INSTALL" == "true" && "$PREBUILT_INSTALLED" == "false" ]]; then
  success "Skipped install per flags (--skip-build --skip-install)"
else
  next_steps=(
    "rantaiclaw chat       — start an interactive session"
    "rantaiclaw agent      — run the autonomous agent loop"
    "rantaiclaw status     — verify installation"
  )
  if [[ "$RUN_ONBOARD" == false ]]; then
    next_steps+=("rantaiclaw onboard --interactive   — connect a provider (OpenRouter, Anthropic, etc.)")
  fi
  print_success_banner "${next_steps[@]}"
fi
