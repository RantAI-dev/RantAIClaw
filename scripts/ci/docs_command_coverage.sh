#!/usr/bin/env bash
# Assert every top-level CLI command is documented in docs/reference/commands.md.
#
# Usage: scripts/ci/docs_command_coverage.sh
#
# Why this exists
# ---------------
# The command reference drifts silently: a command added to the `Commands` enum
# in src/main.rs but not to commands.md leaves operators unable to discover it,
# and nothing catches the gap because the docs-quality CI job only runs when a
# DOC file changed — a pure code change that adds a command triggers no doc gate.
# This check closes that hole from the code side (it runs on `rust_changed`).
#
# How it works
# ------------
# The single source of truth for command names is the top-level `enum Commands`
# in src/main.rs. clap lower-cases each single-word PascalCase variant to form
# the CLI name (no `#[command(name/alias)]` overrides exist on this enum — if one
# is ever added, extend the extraction below). For each derived name we require a
# mention in commands.md: a backtick code-span, a `rantaiclaw <cmd>` usage, or a
# section heading.
#
# ADVISORY BACKLOG: commands already known-missing are listed in KNOWN_MISSING
# with the plan that adds them. A command NOT on that list and NOT in the docs is
# a NEW drift and fails the check. When plans 260-J5/J6 land the full command
# reference, empty KNOWN_MISSING and flip this step to blocking in ci-run.yml.

set -euo pipefail

MAIN="src/main.rs"
DOCS="docs/reference/commands.md"

# cmd<TAB>reason — each MUST name the plan/finding that adds it. Delete the line
# when the command is documented.
KNOWN_MISSING=$(cat <<'EOF'
permissions	plan 260 J5 (command-reference regen) — deferred follow-up
auth	plan 260 J5 (command-reference regen) — deferred follow-up
chat	plan 260 J5 (command-reference regen) — deferred follow-up
rollback	plan 260 J5 (command-reference regen) — deferred follow-up
uninstall	plan 260 J5 (command-reference regen) — deferred follow-up
session	plan 260 J5 (command-reference regen) — deferred follow-up
insights	plan 260 J5 (command-reference regen) — deferred follow-up
personality	plan 260 J5 (command-reference regen) — deferred follow-up
profile	plan 260 J5 (command-reference regen) — deferred follow-up
EOF
)

if [ ! -f "$MAIN" ] || [ ! -f "$DOCS" ]; then
  echo "ERROR: expected $MAIN and $DOCS to exist (run from repo root)." >&2
  exit 2
fi

# Extract top-level command variant names from `enum Commands { ... }`.
# A variant line is a PascalCase identifier at 4-space indent followed by
# `{`, `(`, or `,`. Doc comments and `#[command(...)]` attrs are skipped by the
# leading-uppercase requirement.
commands="$(awk '/^enum Commands \{/{f=1} f && /^\}/{exit} f' "$MAIN" \
  | grep -oE '^    [A-Z][A-Za-z0-9]*( \{|,|\()' \
  | sed -E 's/^[[:space:]]+([A-Za-z0-9]+).*/\1/' \
  | tr 'A-Z' 'a-z' \
  | sort -u)"

if [ -z "$commands" ]; then
  echo "ERROR: parsed zero commands from $MAIN — the enum shape changed; fix this script." >&2
  exit 2
fi

fail=0
missing_new=0
known=0
checked=0

for c in $commands; do
  checked=$((checked + 1))
  # A mention is a backtick span, a `rantaiclaw <cmd>` usage, or a heading word.
  if grep -qE "(\`$c\`|rantaiclaw $c( |\`|\$)|^#+ .*\b$c\b)" "$DOCS"; then
    continue
  fi
  if grep -qP "^\Q$c\E\t" <<<"$KNOWN_MISSING"; then
    reason="$(grep -P "^\Q$c\E\t" <<<"$KNOWN_MISSING" | cut -f2-)"
    echo "known:   $c — $reason"
    known=$((known + 1))
    continue
  fi
  echo "ERROR:   command \`$c\` is in $MAIN but not documented in $DOCS."
  echo "         Add it to the command reference, or (if intentionally undocumented)"
  echo "         list it in KNOWN_MISSING with the plan that resolves it."
  missing_new=$((missing_new + 1))
  fail=1
done

echo "checked $checked command(s); $known known-missing (backlog), $missing_new new drift"

if [ "$fail" -ne 0 ]; then
  echo
  echo "A CLI command is undocumented. See the message(s) above."
  exit 1
fi

exit 0
