#!/usr/bin/env bash
set -euo pipefail

__BOOTSTRAP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd || pwd)"

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

# Defensive trap: kill spinner if bootstrap exits mid-step.
trap '__ui_spinner_kill 2>/dev/null || true' EXIT

usage() {
  cat <<'USAGE'
RantaiClaw installer bootstrap engine

Usage:
  ./rantaiclaw_install.sh [options]
  ./bootstrap.sh [options]         # compatibility entrypoint

Modes:
  Default mode installs/builds RantaiClaw only (requires existing Rust toolchain).
  Guided mode asks setup questions and configures options interactively.
  Optional bootstrap mode can also install system dependencies and Rust.

Options:
  --guided                   Run interactive guided installer
  --no-guided                Disable guided installer
  --docker                   Run bootstrap in Docker-compatible mode and launch onboarding inside the container
  --install-system-deps      Install build dependencies (Linux/macOS)
  --install-rust             Install Rust via rustup if missing
  --prefer-prebuilt          Try latest release binary first; fallback to source build on miss
  --prebuilt-only            Install only from latest release binary (no source build fallback)
  --force-source-build       Disable prebuilt flow and always build from source
  --onboard                  Run onboarding after install
  --interactive-onboard      Run interactive onboarding (implies --onboard)
  --api-key <key>            API key for non-interactive onboarding
  --provider <id>            Provider for non-interactive onboarding (default: openrouter)
  --model <id>               Model for non-interactive onboarding (optional)
  --build-first              Alias for explicitly enabling separate `cargo build --release --locked`
  --skip-build               Skip build step (`cargo build --release --locked` or Docker image build)
  --skip-install             Skip `cargo install --path . --force --locked`
  -h, --help                 Show help

Examples:
  ./rantaiclaw_install.sh
  ./rantaiclaw_install.sh --guided
  ./rantaiclaw_install.sh --install-system-deps --install-rust
  ./rantaiclaw_install.sh --prefer-prebuilt
  ./rantaiclaw_install.sh --prebuilt-only
  ./rantaiclaw_install.sh --onboard --api-key "sk-..." --provider openrouter [--model "openrouter/auto"]
  ./rantaiclaw_install.sh --interactive-onboard

  # Compatibility entrypoint:
  ./bootstrap.sh --docker

  # Remote one-liner
  curl -fsSL https://raw.githubusercontent.com/rantaiclaw-labs/rantaiclaw/main/scripts/bootstrap.sh | bash

Environment:
  RANTAICLAW_CONTAINER_CLI     Container CLI command (default: docker; auto-fallback: podman)
  RANTAICLAW_DOCKER_DATA_DIR   Host path for Docker config/workspace persistence
  RANTAICLAW_DOCKER_IMAGE      Docker image tag to build/run (default: rantaiclaw-bootstrap:local)
  RANTAICLAW_API_KEY           Used when --api-key is not provided
  RANTAICLAW_PROVIDER          Used when --provider is not provided (default: openrouter)
  RANTAICLAW_MODEL             Used when --model is not provided
  RANTAICLAW_BOOTSTRAP_MIN_RAM_MB   Minimum RAM threshold for source build preflight (default: 2048)
  RANTAICLAW_BOOTSTRAP_MIN_DISK_MB  Minimum free disk threshold for source build preflight (default: 6144)
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

install_prebuilt_binary() {
  local target archive_url temp_dir archive_path extracted_bin install_dir

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
    warn "No pre-built binary target mapping for $(uname -s)/$(uname -m)."
    return 1
  fi

  archive_url="https://github.com/rantaiclaw-labs/rantaiclaw/releases/latest/download/rantaiclaw-${target}.tar.gz"
  temp_dir="$(mktemp -d -t rantaiclaw-prebuilt-XXXXXX)"
  archive_path="$temp_dir/rantaiclaw-${target}.tar.gz"

  next_step "Attempting pre-built binary install for target: $target"
  spinner_start "Downloading prebuilt binary for $target"
  if curl -fsSL "$archive_url" -o "$archive_path"; then
    spinner_stop "Downloaded prebuilt binary"
  else
    spinner_stop_fail "Download failed"
    warn "Could not download release asset: $archive_url"
    rm -rf "$temp_dir"
    return 1
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

  install_dir="$HOME/.cargo/bin"
  mkdir -p "$install_dir"
  install -m 0755 "$extracted_bin" "$install_dir/rantaiclaw"
  rm -rf "$temp_dir"

  info "Installed pre-built binary to $install_dir/rantaiclaw"
  if [[ ":$PATH:" != *":$install_dir:"* ]]; then
    warn "$install_dir is not in PATH for this shell."
    warn "Run: export PATH=\"$install_dir:\$PATH\""
  fi

  return 0
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

prompt_yes_no() {
  local question="$1"
  local default_answer="$2"
  local prompt=""
  local answer=""

  if [[ "$default_answer" == "yes" ]]; then
    prompt="[Y/n]"
  else
    prompt="[y/N]"
  fi

  while true; do
    if ! read -r -p "$question $prompt " answer; then
      error "guided installer input was interrupted."
      exit 1
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

  echo
  echo "RantaiClaw guided installer"
  echo "Answer a few questions, then the installer will run automatically."
  echo

  if [[ "$os_name" == "Linux" ]]; then
    if prompt_yes_no "Install Linux build dependencies (toolchain/pkg-config/git/curl)?" "yes"; then
      INSTALL_SYSTEM_DEPS=true
    fi
  else
    if prompt_yes_no "Install system dependencies for $os_name?" "no"; then
      INSTALL_SYSTEM_DEPS=true
    fi
  fi

  if have_cmd cargo && have_cmd rustc; then
    info "Detected Rust toolchain: $(rustc --version)"
  else
    if prompt_yes_no "Rust toolchain not found. Install Rust via rustup now?" "yes"; then
      INSTALL_RUST=true
    fi
  fi

  if prompt_yes_no "Run a separate prebuild before install?" "yes"; then
    SKIP_BUILD=false
  else
    SKIP_BUILD=true
  fi

  if prompt_yes_no "Install rantaiclaw into cargo bin now?" "yes"; then
    SKIP_INSTALL=false
  else
    SKIP_INSTALL=true
  fi

  if prompt_yes_no "Run onboarding after install?" "no"; then
    RUN_ONBOARD=true
    if prompt_yes_no "Use interactive onboarding?" "yes"; then
      INTERACTIVE_ONBOARD=true
    else
      INTERACTIVE_ONBOARD=false
      if ! read -r -p "Provider [$PROVIDER]: " provider_input; then
        error "guided installer input was interrupted."
        exit 1
      fi
      if [[ -n "$provider_input" ]]; then
        PROVIDER="$provider_input"
      fi

      if ! read -r -p "Model [${MODEL:-leave empty}]: " model_input; then
        error "guided installer input was interrupted."
        exit 1
      fi
      if [[ -n "$model_input" ]]; then
        MODEL="$model_input"
      fi

      if [[ -z "$API_KEY" ]]; then
        if ! read -r -s -p "API key (hidden, leave empty to switch to interactive onboarding): " api_key_input; then
          echo
          error "guided installer input was interrupted."
          exit 1
        fi
        echo
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
  info "Installer plan"
  local install_binary=true
  local build_first=false
  if [[ "$SKIP_INSTALL" == true ]]; then
    install_binary=false
  fi
  if [[ "$SKIP_BUILD" == false ]]; then
    build_first=true
  fi
  echo "    docker-mode: $(bool_to_word "$DOCKER_MODE")"
  echo "    install-system-deps: $(bool_to_word "$INSTALL_SYSTEM_DEPS")"
  echo "    install-rust: $(bool_to_word "$INSTALL_RUST")"
  echo "    build-first: $(bool_to_word "$build_first")"
  echo "    install-binary: $(bool_to_word "$install_binary")"
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
  if ! prompt_yes_no "Proceed with this install plan?" "yes"; then
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
  fallback_image="ghcr.io/rantaiclaw-labs/rantaiclaw:latest"
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
REPO_URL="https://github.com/rantaiclaw-labs/rantaiclaw.git"
ORIGINAL_ARG_COUNT=$#
GUIDED_MODE="auto"

DOCKER_MODE=false
INSTALL_SYSTEM_DEPS=false
INSTALL_RUST=false
PREFER_PREBUILT=false
PREBUILT_ONLY=false
FORCE_SOURCE_BUILD=false
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
      PREFER_PREBUILT=true
      shift
      ;;
    --prebuilt-only)
      PREBUILT_ONLY=true
      shift
      ;;
    --force-source-build)
      FORCE_SOURCE_BUILD=true
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
# show a stable progress denominator across the whole install.
compute_step_total() {
  local total=2  # preflight + final install
  [[ "${INSTALL_SYSTEM_DEPS:-false}" == "true" ]] && total=$((total + 1))
  [[ "${INSTALL_RUST:-false}" == "true" ]] && total=$((total + 1))
  if [[ "${PREBUILT_ONLY:-false}" == "true" ]]; then
    total=$((total + 1))   # prebuilt fetch
  elif [[ "${SKIP_BUILD:-false}" != "true" ]]; then
    total=$((total + 2))   # source fetch + build
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
  if [[ "$TEMP_CLONE" == true && -n "$TEMP_DIR" && -d "$TEMP_DIR" ]]; then
    rm -rf "$TEMP_DIR"
  fi
}
trap cleanup EXIT

# Support three launch modes:
# 1) ./bootstrap.sh from repo root
# 2) scripts/bootstrap.sh from repo
# 3) curl | bash (no local repo => temporary clone)
if [[ ! -f "$WORK_DIR/Cargo.toml" ]]; then
  if [[ -f "$(pwd)/Cargo.toml" ]]; then
    WORK_DIR="$(pwd)"
  else
    if ! have_cmd git; then
      error "git is required when running bootstrap outside a local repository checkout."
      if [[ "$INSTALL_SYSTEM_DEPS" == false ]]; then
        error "Re-run with --install-system-deps or install git manually."
      fi
      exit 1
    fi

    TEMP_DIR="$(mktemp -d -t rantaiclaw-bootstrap-XXXXXX)"
    info "No local repository detected; cloning latest main branch"
    git clone --depth 1 "$REPO_URL" "$TEMP_DIR"
    WORK_DIR="$TEMP_DIR"
    TEMP_CLONE=true
  fi
fi

next_step "RantaiClaw bootstrap"
echo "    workspace: $WORK_DIR"

cd "$WORK_DIR"

if [[ "$FORCE_SOURCE_BUILD" == true ]]; then
  PREFER_PREBUILT=false
  PREBUILT_ONLY=false
fi

if [[ "$PREBUILT_ONLY" == true ]]; then
  PREFER_PREBUILT=true
fi

if [[ "$DOCKER_MODE" == true ]]; then
  ensure_docker_ready
  if [[ "$RUN_ONBOARD" == false ]]; then
    RUN_ONBOARD=true
    if [[ -z "$API_KEY" ]]; then
      INTERACTIVE_ONBOARD=true
    fi
  fi
  run_docker_bootstrap
  cat <<'DONE'

✅ Docker bootstrap complete.

Your containerized RantaiClaw data is persisted under:
DONE
  echo "  $DOCKER_DATA_DIR"
  cat <<'DONE'

Next steps:
  ./rantaiclaw_install.sh --docker --interactive-onboard
  ./rantaiclaw_install.sh --docker --api-key "sk-..." --provider openrouter
DONE
  exit 0
fi

if [[ "$FORCE_SOURCE_BUILD" == false ]]; then
  if [[ "$PREFER_PREBUILT" == false && "$PREBUILT_ONLY" == false ]]; then
    if should_attempt_prebuilt_for_resources "$WORK_DIR"; then
      info "Attempting pre-built binary first due to resource preflight."
      PREFER_PREBUILT=true
    fi
  fi

  if [[ "$PREFER_PREBUILT" == true ]]; then
    if install_prebuilt_binary; then
      PREBUILT_INSTALLED=true
      SKIP_BUILD=true
      SKIP_INSTALL=true
    elif [[ "$PREBUILT_ONLY" == true ]]; then
      error "Pre-built-only mode requested, but no compatible release asset is available."
      error "Try again later, or run with --force-source-build on a machine with enough RAM/disk."
      exit 1
    else
      warn "Pre-built install unavailable; falling back to source build."
    fi
  fi
fi

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

cat <<'DONE'

✅ Bootstrap complete.

Next steps:
  rantaiclaw status
  rantaiclaw agent -m "Hello, RantaiClaw!"
  rantaiclaw gateway
DONE
