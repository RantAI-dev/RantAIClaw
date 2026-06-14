#!/usr/bin/env bash
# Run the locally-built rantaiclaw in a PERSISTENT, ISOLATED sandbox.
#
# Everything (profiles, config, sessions, memory DB, UI, snapshots) is redirected
# under ./dev-sandbox/home, so testing the unified-agent-runtime branch NEVER
# touches your real ~/.rantaiclaw, ~/.local/share, or ~/.config.
#
# Isolation levers (verified against the source):
#   HOME            -> ~/.rantaiclaw profiles, /ui, /.update-snapshots, telegram session
#   XDG_DATA_HOME   -> gateway/CLI session store + on-disk memory (dirs::data_dir)
#   XDG_CONFIG_HOME -> dirs::config_dir
#   RANTAICLAW_PROFILE -> selects the sandbox profile
#
# Usage:
#   dev/sandbox.sh                  # launch the TUI in the sandbox
#   dev/sandbox.sh agent -m "hi"    # one-shot agent turn (PR2 loop)
#   dev/sandbox.sh gateway          # run the gateway (PR3 owner-gate / SSE)
#   dev/sandbox.sh memory list      # inspect sandbox memory (PR4 scoping)
#   dev/sandbox.sh doctor           # no-config smoke check
#
# Real LLM turns need a key:
#   RANTAICLAW_SANDBOX_API_KEY=sk-... dev/sandbox.sh agent -m "what is 2+2?"
#
# Force a rebuild:   RANTAICLAW_SANDBOX_REBUILD=1 dev/sandbox.sh ...
# Reset the sandbox: rm -rf dev-sandbox
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SANDBOX="${RANTAICLAW_SANDBOX_DIR:-$ROOT_DIR/dev-sandbox}"
PROFILE="${RANTAICLAW_PROFILE:-sandbox}"
BIN="${RANTAICLAW_BIN:-$ROOT_DIR/target/debug/rantaiclaw}"

# Build with the REAL environment first so cargo's ~/.cargo cache is used,
# BEFORE we repoint HOME at the sandbox (otherwise cargo would re-fetch crates
# into the sandbox HOME).
if [ ! -x "$BIN" ] || [ "${RANTAICLAW_SANDBOX_REBUILD:-0}" = "1" ]; then
  echo "building dev binary (real HOME, cargo cache intact)…" >&2
  ( cd "$ROOT_DIR" && cargo build --bin rantaiclaw )
fi

SBHOME="$SANDBOX/home"
PROFILE_DIR="$SBHOME/.rantaiclaw/profiles/$PROFILE"
mkdir -p "$PROFILE_DIR/workspace" "$SBHOME/.local/share" "$SBHOME/.config"

# Seed a minimal config + active profile on first run only — never clobber a
# config you've edited / onboarded in the sandbox.
if [ ! -f "$PROFILE_DIR/config.toml" ]; then
  cat >"$PROFILE_DIR/config.toml" <<EOF
api_key = "${RANTAICLAW_SANDBOX_API_KEY:-sandbox-placeholder-key}"
default_provider = "${RANTAICLAW_SANDBOX_PROVIDER:-openrouter}"
default_model = "${RANTAICLAW_SANDBOX_MODEL:-anthropic/claude-sonnet-4-20250514}"
default_temperature = 0.7

# ── unified-agent-runtime test knobs ───────────────────────────────────────
[channels_config]
cli = true
# PR3 owner-gate: sender ids allowed to APPROVE tool calls over a channel.
# Empty = secure default (approval-required tools auto-deny). Add your channel
# sender id (e.g. a Telegram user id) here to test that only an owner's Y/A is
# honored. "*" = any sender (insecure; for testing only).
approval_owners = []
EOF
  printf '%s\n' "$PROFILE" >"$SBHOME/.rantaiclaw/active_profile"
  echo "seeded sandbox profile '$PROFILE' -> $PROFILE_DIR/config.toml" >&2
fi

export HOME="$SBHOME"
export XDG_DATA_HOME="$SBHOME/.local/share"
export XDG_CONFIG_HOME="$SBHOME/.config"
export XDG_STATE_HOME="$SBHOME/.local/state"
export XDG_CACHE_HOME="$SBHOME/.cache"
export RANTAICLAW_PROFILE="$PROFILE"
export RANTAICLAW_LOG_STDERR=1

echo "→ sandbox HOME=$HOME  profile=$PROFILE  (your real ~/.rantaiclaw is untouched)" >&2
exec "$BIN" "$@"
