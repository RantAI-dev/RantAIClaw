#!/usr/bin/env bash
set -euo pipefail

if [ -f "dev/docker-compose.ci.yml" ]; then
  COMPOSE_FILE="dev/docker-compose.ci.yml"
elif [ -f "docker-compose.ci.yml" ] && [ "$(basename "$(pwd)")" = "dev" ]; then
  COMPOSE_FILE="docker-compose.ci.yml"
else
  echo "❌ Run this script from repo root or dev/ directory."
  exit 1
fi

compose_cmd=(docker compose -f "$COMPOSE_FILE")
SMOKE_CACHE_DIR="${SMOKE_CACHE_DIR:-.cache/buildx-smoke}"

run_in_ci() {
  local cmd="$1"
  "${compose_cmd[@]}" run --rm local-ci bash -c "$cmd"
}

build_smoke_image() {
  if docker buildx version >/dev/null 2>&1; then
    mkdir -p "$SMOKE_CACHE_DIR"
    local build_args=(
      --load
      --target dev
      --cache-to "type=local,dest=$SMOKE_CACHE_DIR,mode=max"
      -t rantaiclaw-local-smoke:latest
      .
    )
    if [ -f "$SMOKE_CACHE_DIR/index.json" ]; then
      build_args=(--cache-from "type=local,src=$SMOKE_CACHE_DIR" "${build_args[@]}")
    fi
    docker buildx build "${build_args[@]}"
  else
    DOCKER_BUILDKIT=1 docker build --target dev -t rantaiclaw-local-smoke:latest .
  fi
}

run_tui_smoke() {
  if ! command -v tmux >/dev/null 2>&1; then
    echo "⚠ tmux not installed; skipping TUI smoke."
    return 0
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    echo "⚠ cargo not installed on host; skipping TUI smoke."
    return 0
  fi
  cargo build --bin rantaiclaw
  RANTAICLAW_BIN="$PWD/target/debug/rantaiclaw" bash dev/tui-smoke.sh
}

print_help() {
  cat <<'EOF'
RantaiClaw Local CI in Docker

Usage: ./dev/ci.sh <command>

Commands:
  build-image   Build/update the local CI image
  shell         Open an interactive shell inside the CI container
  lint          Run rustfmt + clippy correctness gate (container only)
  lint-strict   Run rustfmt + full clippy warnings gate (container only)
  lint-delta    Run strict lint delta gate on changed Rust lines (container only)
  test          Run cargo test (container only)
  build         Run release build smoke check (container only)
  audit         Run cargo audit (container only)
  deny          Run cargo deny check (container only)
  security      Run cargo audit + cargo deny (container only)
  tui-smoke     Run tmux-backed TUI smoke test (host; skips without tmux)
  docker-smoke  Build and verify runtime image (host docker daemon)
  all           Run lint, test, build, security, tui-smoke, docker-smoke
  clean         Remove local CI containers and volumes
EOF
}

if [ $# -lt 1 ]; then
  print_help
  exit 1
fi

case "$1" in
  build-image)
    "${compose_cmd[@]}" build local-ci
    ;;

  shell)
    "${compose_cmd[@]}" run --rm local-ci bash
    ;;

  lint)
    run_in_ci "./scripts/ci/rust_quality_gate.sh"
    ;;

  lint-strict)
    run_in_ci "./scripts/ci/rust_quality_gate.sh --strict"
    ;;

  lint-delta)
    run_in_ci "./scripts/ci/rust_strict_delta_gate.sh"
    ;;

  test)
    run_in_ci "cargo test --locked --verbose"
    ;;

  build)
    run_in_ci "cargo build --release --locked --verbose"
    ;;

  audit)
    run_in_ci "cargo audit"
    ;;

  deny)
    run_in_ci "cargo deny check licenses sources"
    ;;

  security)
    run_in_ci "cargo deny check licenses sources"
    run_in_ci "cargo audit"
    ;;

  tui-smoke)
    run_tui_smoke
    ;;

  docker-smoke)
    build_smoke_image
    docker run --rm rantaiclaw-local-smoke:latest --version
    ;;

  all)
    run_in_ci "./scripts/ci/rust_quality_gate.sh"
    run_in_ci "cargo test --locked --verbose"
    run_in_ci "cargo build --release --locked --verbose"
    run_in_ci "cargo deny check licenses sources"
    run_in_ci "cargo audit"
    run_tui_smoke
    build_smoke_image
    docker run --rm rantaiclaw-local-smoke:latest --version
    ;;

  clean)
    "${compose_cmd[@]}" down -v --remove-orphans
    ;;

  *)
    print_help
    exit 1
    ;;
esac
