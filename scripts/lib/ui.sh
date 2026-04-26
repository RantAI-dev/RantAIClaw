# scripts/lib/ui.sh
# RantaiClaw installer UX helpers — colors, glyphs, banners, prompts.
# Sourced by scripts/bootstrap.sh. Pure presentation; no side effects.
#
# Public functions:
#   info "msg"          — cyan arrow + message (stdout)
#   success "msg"       — green check + message (stdout)
#   warn "msg"          — yellow warn + message (stderr)
#   error "msg"         — red cross + message (stderr)
#   print_banner        — opening framed banner
#   print_success_banner [next_step ...]
#                       — closing framed banner with optional next-step lines
#   step "N/T" "title"  — bold cyan [N/T] step label
#   spinner_start "msg" — start braille spinner with message
#   spinner_stop "msg"  — stop spinner, print success line
#   spinner_stop_fail "msg"
#                       — stop spinner, print error line
#   prompt_yes_no "Q?" "yes|no"
#                       — pipe-safe yes/no prompt; returns 0 for yes, 1 for no
#   prompt_input "Q" "default"
#                       — pipe-safe input prompt; echoes value
#   prompt_input_secret "Q"
#                       — pipe-safe hidden input prompt; echoes value
#
# Color detection:
#   Colors emitted iff: stdout is a TTY ([ -t 1 ])
#                  AND NO_COLOR env unset
#                  AND TERM != "dumb" and not unset.
#   Override:    __UI_FORCE_COLOR=1 forces colors on (used in tests).
#   Glyphs:      always Unicode (→ ✓ ⚠ ✗ •); modern terminals render them.

# Detect color support once; result stored in __UI_COLOR.
__ui_detect_color() {
  if [[ "${__UI_FORCE_COLOR:-0}" == "1" ]]; then
    __UI_COLOR=1
    return
  fi
  if [[ -n "${NO_COLOR:-}" ]]; then __UI_COLOR=0; return; fi
  if [[ -z "${TERM:-}" || "${TERM}" == "dumb" ]]; then __UI_COLOR=0; return; fi
  if [[ ! -t 1 ]]; then __UI_COLOR=0; return; fi
  __UI_COLOR=1
}
__ui_detect_color

if [[ "$__UI_COLOR" == "1" ]]; then
  __UI_RED=$'\033[0;31m'
  __UI_GREEN=$'\033[0;32m'
  __UI_YELLOW=$'\033[0;33m'
  __UI_BLUE=$'\033[0;34m'
  __UI_MAGENTA=$'\033[0;35m'
  __UI_CYAN=$'\033[0;36m'
  __UI_BOLD=$'\033[1m'
  __UI_RESET=$'\033[0m'
else
  __UI_RED=''
  __UI_GREEN=''
  __UI_YELLOW=''
  __UI_BLUE=''
  __UI_MAGENTA=''
  __UI_CYAN=''
  __UI_BOLD=''
  __UI_RESET=''
fi

info() {
  printf '%s→%s %s\n' "$__UI_CYAN" "$__UI_RESET" "$*"
}

success() {
  printf '%s✓%s %s\n' "$__UI_GREEN" "$__UI_RESET" "$*"
}

warn() {
  printf '%s⚠%s %s\n' "$__UI_YELLOW" "$__UI_RESET" "$*" >&2
}

error() {
  printf '%s✗%s %s\n' "$__UI_RED" "$__UI_RESET" "$*" >&2
}

# Box-drawing banner — fixed 59-column inner width matches Hermes' style.
# Avoids dynamic `tput cols` so output is stable under non-TTY widths.
__UI_BANNER_INNER='─────────────────────────────────────────────────────────'

print_banner() {
  printf '\n'
  printf '%s%s┌%s┐%s\n' "$__UI_MAGENTA" "$__UI_BOLD" "$__UI_BANNER_INNER" "$__UI_RESET"
  printf '%s%s│            ⚙ RantaiClaw Installer                       │%s\n' \
    "$__UI_MAGENTA" "$__UI_BOLD" "$__UI_RESET"
  printf '%s%s├%s┤%s\n' "$__UI_MAGENTA" "$__UI_BOLD" "$__UI_BANNER_INNER" "$__UI_RESET"
  printf '%s%s│  Multi-Agent Runtime for Production AI Employees        │%s\n' \
    "$__UI_MAGENTA" "$__UI_BOLD" "$__UI_RESET"
  printf '%s%s└%s┘%s\n' "$__UI_MAGENTA" "$__UI_BOLD" "$__UI_BANNER_INNER" "$__UI_RESET"
  printf '\n'
}

# print_success_banner [next_step ...]
# Each argument becomes one bullet line under the banner.
print_success_banner() {
  printf '\n'
  printf '%s%s┌%s┐%s\n' "$__UI_GREEN" "$__UI_BOLD" "$__UI_BANNER_INNER" "$__UI_RESET"
  printf '%s%s│            ✓ Installation Complete!                     │%s\n' \
    "$__UI_GREEN" "$__UI_BOLD" "$__UI_RESET"
  printf '%s%s└%s┘%s\n' "$__UI_GREEN" "$__UI_BOLD" "$__UI_BANNER_INNER" "$__UI_RESET"
  if [[ "$#" -gt 0 ]]; then
    printf '\n%s→%s Next steps:\n' "$__UI_CYAN" "$__UI_RESET"
    for line in "$@"; do
      printf '  %s•%s %s\n' "$__UI_CYAN" "$__UI_RESET" "$line"
    done
  fi
  printf '\n'
}

# step "N/T" "title" — bold cyan [N/T] step label, plain title after.
step() {
  local progress="$1"
  shift
  printf '\n%s%s[%s]%s %s\n' "$__UI_BOLD" "$__UI_CYAN" "$progress" "$__UI_RESET" "$*"
}

# Braille spinner. Backgrounded subshell + carriage-return overwrite.
# Falls back to a plain info line when stdout is not a TTY (capture-safe).
__UI_SPINNER_PID=""
__UI_SPINNER_FRAMES='⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏'

spinner_start() {
  local msg="$*"
  if [[ "$__UI_COLOR" != "1" || ! -t 1 ]]; then
    info "$msg…"
    __UI_SPINNER_PID=""
    return
  fi
  (
    local i=0
    local frame
    while :; do
      frame="${__UI_SPINNER_FRAMES:i:1}"
      i=$(((i + 1) % ${#__UI_SPINNER_FRAMES}))
      printf '\r%s%s%s %s' "$__UI_CYAN" "$frame" "$__UI_RESET" "$msg" >&2
      sleep 0.1
    done
  ) &
  __UI_SPINNER_PID=$!
  disown "$__UI_SPINNER_PID" 2>/dev/null || true
}

__ui_spinner_kill() {
  if [[ -n "$__UI_SPINNER_PID" ]]; then
    kill "$__UI_SPINNER_PID" 2>/dev/null || true
    wait "$__UI_SPINNER_PID" 2>/dev/null || true
    printf '\r\033[K' >&2
    __UI_SPINNER_PID=""
  fi
}

spinner_stop() {
  __ui_spinner_kill
  success "$*"
}

spinner_stop_fail() {
  __ui_spinner_kill
  error "$*"
}

# IS_INTERACTIVE detects whether stdin is a TTY at script start.
# Sourced scripts re-detect; consumers may override after sourcing.
if [[ -t 0 ]]; then
  IS_INTERACTIVE=true
else
  IS_INTERACTIVE=false
fi

# prompt_yes_no "Question?" "yes|no" — returns 0 for yes, 1 for no.
# Default is used when input is empty or unavailable.
prompt_yes_no() {
  local question="$1"
  local default="${2:-yes}"
  local prompt_suffix
  case "$default" in
    [yY]|[yY][eE][sS]|1|true|TRUE) prompt_suffix="[Y/n]" ;;
    *) prompt_suffix="[y/N]" ;;
  esac

  local answer=""
  if [[ "$IS_INTERACTIVE" == "true" ]]; then
    read -r -p "$question $prompt_suffix " answer || answer=""
  elif [[ -r /dev/tty && -w /dev/tty ]]; then
    printf '%s %s ' "$question" "$prompt_suffix" > /dev/tty
    IFS= read -r answer < /dev/tty || answer=""
  else
    answer=""
  fi

  # Trim surrounding whitespace.
  answer="${answer#"${answer%%[![:space:]]*}"}"
  answer="${answer%"${answer##*[![:space:]]}"}"

  if [[ -z "$answer" ]]; then
    case "$default" in
      [yY]|[yY][eE][sS]|1|true|TRUE) return 0 ;;
      *) return 1 ;;
    esac
  fi
  case "$answer" in
    [yY]|[yY][eE][sS]) return 0 ;;
    *) return 1 ;;
  esac
}

# prompt_input "Question" "default" — echoes the captured value (or default).
prompt_input() {
  local question="$1"
  local default="${2:-}"
  local suffix=""
  [[ -n "$default" ]] && suffix=" [$default]"

  local answer=""
  if [[ "$IS_INTERACTIVE" == "true" ]]; then
    read -r -p "$question$suffix: " answer || answer=""
  elif [[ -r /dev/tty && -w /dev/tty ]]; then
    printf '%s%s: ' "$question" "$suffix" > /dev/tty
    IFS= read -r answer < /dev/tty || answer=""
  else
    answer=""
  fi
  answer="${answer#"${answer%%[![:space:]]*}"}"
  answer="${answer%"${answer##*[![:space:]]}"}"
  [[ -z "$answer" ]] && answer="$default"
  printf '%s' "$answer"
}

# prompt_input_secret "Question" — echoes captured value, hidden during entry.
prompt_input_secret() {
  local question="$1"
  local answer=""
  if [[ "$IS_INTERACTIVE" == "true" ]]; then
    read -r -s -p "$question: " answer || answer=""
    printf '\n' >&2
  elif [[ -r /dev/tty && -w /dev/tty ]]; then
    printf '%s: ' "$question" > /dev/tty
    stty -echo < /dev/tty 2>/dev/null || true
    IFS= read -r answer < /dev/tty || answer=""
    stty echo < /dev/tty 2>/dev/null || true
    printf '\n' > /dev/tty
  else
    answer=""
  fi
  printf '%s' "$answer"
}
