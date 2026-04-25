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
# Use `env` to set __UI_FORCE_COLOR for the bash -c invocation — avoids
# reliance on special-builtin assignment semantics inside the subshell.
out=$(env -u NO_COLOR __UI_FORCE_COLOR=1 bash -c "source '$UI_LIB' && info 'colored'")
assert_contains $'\033[' "$out" 'info emits ANSI escape when forced color'
assert_contains 'colored' "$out" 'info still includes message under color'

# Banners.
out=$(NO_COLOR=1 bash -c "source '$UI_LIB' && print_banner")
assert_contains 'RantaiClaw Installer' "$out" 'print_banner contains title'
assert_contains '┌' "$out" 'print_banner contains box-drawing top'
assert_contains '└' "$out" 'print_banner contains box-drawing bottom'

out=$(NO_COLOR=1 bash -c "source '$UI_LIB' && print_success_banner 'Run rantaiclaw chat'")
assert_contains 'Installation Complete' "$out" 'success banner contains title'
assert_contains 'Run rantaiclaw chat' "$out" 'success banner includes next step'

out=$(NO_COLOR=1 bash -c "source '$UI_LIB' && step '3/7' 'Installing system deps'")
assert_contains '[3/7]' "$out" 'step shows N/T in brackets'
assert_contains 'Installing system deps' "$out" 'step shows title'

# Spinner: in non-TTY mode (under bash -c capture), should fall back to plain info-style line.
out=$(NO_COLOR=1 bash -c "
  source '$UI_LIB'
  spinner_start 'Building'
  spinner_stop 'Built rantaiclaw'
")
assert_contains 'Built rantaiclaw' "$out" 'spinner_stop produces success line'

out=$(NO_COLOR=1 bash -c "
  source '$UI_LIB'
  spinner_start 'Building'
  spinner_stop_fail 'Build failed'
" 2>&1)
assert_contains 'Build failed' "$out" 'spinner_stop_fail produces error line'

# prompt_yes_no — non-interactive (no /dev/tty available) returns default.
out=$(NO_COLOR=1 bash -c "
  source '$UI_LIB'
  if prompt_yes_no 'Continue?' 'yes' </dev/null; then echo YES; else echo NO; fi
" 2>/dev/null)
assert_contains 'YES' "$out" 'prompt_yes_no returns yes-default under non-interactive stdin'

out=$(NO_COLOR=1 bash -c "
  source '$UI_LIB'
  if prompt_yes_no 'Continue?' 'no' </dev/null; then echo YES; else echo NO; fi
" 2>/dev/null)
assert_contains 'NO' "$out" 'prompt_yes_no returns no-default under non-interactive stdin'

# prompt_input — non-interactive returns default.
out=$(NO_COLOR=1 bash -c "
  source '$UI_LIB'
  prompt_input 'Provider' 'openrouter' </dev/null
" 2>/dev/null)
assert_contains 'openrouter' "$out" 'prompt_input returns default under non-interactive stdin'

printf '\n%d pass, %d fail\n' "$pass" "$fail"
[[ "$fail" -eq 0 ]]
