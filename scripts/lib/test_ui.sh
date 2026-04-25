#!/usr/bin/env bash
# Minimal smoke tests for scripts/lib/ui.sh.
# Run from repo root: bash scripts/lib/test_ui.sh
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UI_LIB="$SCRIPT_DIR/ui.sh"

fail=0
pass=0

assert_contains() {
  local needle="$1"
  local haystack="$2"
  local label="$3"
  if [[ "$haystack" == *"$needle"* ]]; then
    pass=$((pass + 1))
    printf '  ok   %s\n' "$label"
  else
    fail=$((fail + 1))
    printf '  FAIL %s\n  expected to contain: %q\n  got: %q\n' "$label" "$needle" "$haystack"
  fi
}

# Color-off path: NO_COLOR=1 forces empty color vars; glyphs still emitted.
out=$(NO_COLOR=1 bash -c "source '$UI_LIB' && info 'hello'" 2>&1)
assert_contains '→ hello' "$out" 'info emits arrow + message under NO_COLOR'

out=$(NO_COLOR=1 bash -c "source '$UI_LIB' && success 'done'" 2>&1)
assert_contains '✓ done' "$out" 'success emits check + message under NO_COLOR'

out=$(NO_COLOR=1 bash -c "source '$UI_LIB' && warn 'careful'" 2>&1)
assert_contains '⚠ careful' "$out" 'warn emits warning + message under NO_COLOR'

out=$(NO_COLOR=1 bash -c "source '$UI_LIB' && error 'oops'" 2>&1)
assert_contains '✗ oops' "$out" 'error emits cross + message under NO_COLOR'

# Color-on path simulated by clearing NO_COLOR and forcing __UI_COLOR=1.
out=$(unset NO_COLOR; bash -c "__UI_FORCE_COLOR=1 source '$UI_LIB' && info 'colored'")
assert_contains $'\033[' "$out" 'info emits ANSI escape when forced color'
assert_contains 'colored' "$out" 'info still includes message under color'

printf '\n%d pass, %d fail\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
