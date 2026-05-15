#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${RANTAICLAW_BIN:-$ROOT_DIR/target/debug/rantaiclaw}"
SESSION="rantaiclaw-tui-smoke-$$"
PROFILE="tui-smoke-$$"
TMP_HOME="$(mktemp -d)"
CAPTURE="$TMP_HOME/tui-capture.txt"

cleanup() {
  tmux kill-session -t "$SESSION" >/dev/null 2>&1 || true
  rm -rf "$TMP_HOME"
}
trap cleanup EXIT

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 127
  fi
}

need tmux

if [ -z "${RANTAICLAW_BIN:-}" ]; then
  echo "building debug binary..."
  cargo build --bin rantaiclaw
elif [ ! -x "$BIN" ]; then
  echo "building debug binary..."
  cargo build --bin rantaiclaw
fi

mkdir -p "$TMP_HOME/.rantaiclaw/profiles/$PROFILE/workspace"
mkdir -p "$TMP_HOME/.rantaiclaw/profiles/$PROFILE/skills/gog"
cat >"$TMP_HOME/.rantaiclaw/profiles/$PROFILE/config.toml" <<EOF
api_key = "tui-smoke-placeholder"
default_provider = "openrouter"
default_model = "anthropic/claude-sonnet-4-20250514"
default_temperature = 0.7
EOF
cat >"$TMP_HOME/.rantaiclaw/profiles/$PROFILE/skills/gog/SKILL.md" <<'EOF'
---
name: gog
description: Gated smoke skill
version: 0.1.0
metadata: {"clawdbot":{"requires":{"bins":["definitely-missing-rantaiclaw-gog-smoke"]},"install":[{"id":"smoke-fail","kind":"smoke-fail","bins":["definitely-missing-rantaiclaw-gog-smoke"],"label":"Fail fast smoke recipe"}]}}
---
# gog

Smoke fixture for gated skill rendering.
EOF
printf '%s\n' "$PROFILE" >"$TMP_HOME/.rantaiclaw/active_profile"

tmux new-session -d -s "$SESSION" \
  "cd '$ROOT_DIR' && HOME='$TMP_HOME' RANTAICLAW_PROFILE='$PROFILE' RANTAICLAW_LOG_STDERR=1 '$BIN'"

sleep 2
tmux send-keys -t "$SESSION" "/skills" Enter
sleep 2
tmux capture-pane -t "$SESSION" -pS -200 >"$CAPTURE"

if ! grep -Eiq "skills|web[- ]?search|summarizer|meeting" "$CAPTURE"; then
  echo "TUI smoke failed: /skills surface did not render expected content" >&2
  echo "--- captured pane ---" >&2
  cat "$CAPTURE" >&2
  exit 1
fi

tmux send-keys -t "$SESSION" "gog"
sleep 1
tmux capture-pane -t "$SESSION" -pS -200 >"$CAPTURE"

if grep -Fq "No matches for 'gog'" "$CAPTURE"; then
  echo "TUI smoke failed: gated skill was filtered out of /skills" >&2
  echo "--- captured pane ---" >&2
  cat "$CAPTURE" >&2
  exit 1
fi

if ! grep -Fiq "gog" "$CAPTURE" \
  || ! grep -Fiq "gated: missing binary" "$CAPTURE"; then
  echo "TUI smoke failed: gated skill row did not render in /skills" >&2
  echo "--- captured pane ---" >&2
  cat "$CAPTURE" >&2
  exit 1
fi

tmux send-keys -t "$SESSION" Tab
sleep 1
tmux capture-pane -t "$SESSION" -pS -200 >"$CAPTURE"

if ! grep -Fiq "install-deps failed" "$CAPTURE"; then
  echo "TUI smoke failed: install-deps result title did not persist" >&2
  echo "--- captured pane ---" >&2
  cat "$CAPTURE" >&2
  exit 1
fi

if grep -Fiq "Skills · reloaded" "$CAPTURE"; then
  echo "TUI smoke failed: watcher reload clobbered install-deps result title" >&2
  echo "--- captured pane ---" >&2
  cat "$CAPTURE" >&2
  exit 1
fi

tmux send-keys -t "$SESSION" Escape
sleep 1
tmux send-keys -t "$SESSION" C-c

echo "TUI smoke passed: launched TUI, rendered gated skills, and preserved install-deps result title"
