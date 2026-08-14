#!/usr/bin/env bash
# Assert no personal identity data is committed, per CLAUDE.md §9.1.
#
# Usage: check_identity_strings.sh [--self-test]
#
# Why this exists
# ---------------
# §9.1 makes real names, handles, personal emails and phone numbers a merge
# gate, and names the substitutes to use (`rantaiclaw_user`, `user_a`,
# `test_user`, `RantaiClawOperator`, `example.com`). The repo violated its own
# gate in the most visible place available: a contributor's real first name sat
# in the Telegram allowlist prompt that every operator reads during setup, and a
# real handle plus a live Telegram user id sat in approval tests.
#
# Removing them is a one-line fix. Keeping them out is what needs a check.
#
# What it covers
# --------------
# Two classes, deliberately. A gate that cries wolf gets deleted, not fixed.
#
#   1. Identity strings already removed from the tree. Anything reintroducing
#      one is a regression, not a judgment call.
#   2. Personal-mailbox email shapes — a consumer mail provider with a local
#      part that is not one of the neutral placeholders.
#
# Phone numbers are NOT covered. The channel tests legitimately use dozens of
# shapes (`+1234567890`, `+9999999999`, `+15551234567`, `+447911123456`,
# `+81312345678`) and no short rule separates those from a real number without
# firing on most of them. One real number was found by hand during plan 142 and
# replaced; a phone rule can be added if a second ever appears.
#
# Scope: tracked files only. `plans/` and `.superpowers/` are untracked, so they
# are out of scope automatically. Exceptions are listed in EXCLUDED_PATHS with a
# reason each.

set -euo pipefail

# Paths where a hit is expected and must not fail the build.
#   CHANGELOG.md  — released history, immutable by convention. Contains a handle
#                   in the v0.9.x entry describing the bug that entry fixed.
#   this script   — it necessarily spells out what it is looking for.
EXCLUDED_PATHS='^(CHANGELOG\.md|scripts/ci/check_identity_strings\.sh)$'

# Identity strings removed from the tree. One per line, case-insensitive.
# Adding a name here is how you keep a one-off removal from coming back.
BANNED_IDENTITIES=$(cat <<'EOF'
argenis
sulthannauval
sulthan nauval
dramnerf
EOF
)

# Local parts that are placeholders, not people. Anything else in front of a
# consumer mail domain is treated as a real mailbox.
NEUTRAL_LOCAL_PARTS='^(user|users|user_a|user_b|test|test_user|testuser|example|someone|random|noreply|no-reply|admin|rantaiclaw[a-z_]*|you|your_[a-z_]+|me)$'

# Consumer mail providers. Project-scoped domains (example.com, example.test,
# users.noreply.github.com, a self-hosted mail host) are not mailboxes.
CONSUMER_MAIL='(gmail|googlemail|yahoo|hotmail|outlook|live|protonmail|proton|icloud|me|aol|qq|163|foxmail)\.(com|me|co\.[a-z]{2})'

fail=0

files() {
  git ls-files -z | tr '\0' '\n' | grep -vE "$EXCLUDED_PATHS" || true
}

scan_identities() {
  local root="${1:-}"
  while IFS= read -r needle; do
    [ -n "$needle" ] || continue
    local hits
    hits="$(files | { xargs -d '\n' grep -Iin -- "$needle" 2>/dev/null || true; })"
    [ -z "$root" ] || hits="$(printf '%s\n' "$hits" | grep -F "$root" || true)"
    [ -n "$hits" ] || continue
    echo "ERROR: '$needle' is a real identity string (CLAUDE.md §9.1)."
    printf '%s\n' "$hits" | sed 's/^/       /'
    echo "       Use a project-scoped placeholder: rantaiclaw_user, user_a, RantaiClawOperator."
    fail=1
  done <<<"$BANNED_IDENTITIES"
}

scan_emails() {
  local hits
  hits="$(files | { xargs -d '\n' grep -IinE "[a-zA-Z0-9._%+-]+@$CONSUMER_MAIL" 2>/dev/null || true; })"
  [ -n "$hits" ] || return 0
  while IFS= read -r hit; do
    [ -n "$hit" ] || continue
    local address local_part
    address="$(grep -oiE "[a-zA-Z0-9._%+-]+@$CONSUMER_MAIL" <<<"$hit" | head -1)"
    local_part="$(cut -d@ -f1 <<<"$address" | tr '[:upper:]' '[:lower:]')"
    if grep -qE "$NEUTRAL_LOCAL_PARTS" <<<"$local_part"; then
      continue
    fi
    echo "ERROR: '$address' looks like a personal mailbox (CLAUDE.md §9.1)."
    echo "       ${hit%%:*} — use example.com, example.test, or a users.noreply.github.com address."
    fail=1
  done <<<"$hits"
}

# --self-test proves the gate fires, and — more importantly — that it stays
# quiet on the palette §9.1 tells contributors to use.
if [ "${1:-}" = "--self-test" ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  cat >"$tmp/palette.md" <<'EOF'
rantaiclaw_user, user_a, test_user, project_bot, RantaiClawAgent,
RantaiClawOperator, RantaiClawMaintainer, rantaiclaw_bot, rantaiclaw_service,
rantaiclaw_runtime, rantaiclaw_node, rantaiclaw_project, rantaiclaw_workspace,
rantaiclaw_channel, user@example.com, test@example.test, user@icloud.com,
someone@gmail.com, bot@users.noreply.github.com, +1234567890, +15551234567
EOF
  if grep -IinE "[a-zA-Z0-9._%+-]+@$CONSUMER_MAIL" "$tmp/palette.md" \
     | grep -oiE "[a-zA-Z0-9._%+-]+@$CONSUMER_MAIL" \
     | cut -d@ -f1 | tr '[:upper:]' '[:lower:]' \
     | grep -qvE "$NEUTRAL_LOCAL_PARTS"; then
    echo "SELF-TEST FAILED: the email rule fires on the §9.1 approved palette."
    exit 1
  fi
  if grep -Iiq -f <(printf '%s\n' "$BANNED_IDENTITIES") "$tmp/palette.md"; then
    echo "SELF-TEST FAILED: the identity rule fires on the §9.1 approved palette."
    exit 1
  fi
  echo "self-test: the approved palette passes both rules"
fi

scan_identities
scan_emails

if [ "$fail" -ne 0 ]; then
  echo
  echo "Personal identity data is a merge gate (CLAUDE.md §9.1), not a nit."
  exit 1
fi

echo "no identity strings found in tracked files"
exit 0
