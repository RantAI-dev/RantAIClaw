#!/usr/bin/env bash
# Assert every config field is actually read by some runtime code.
#
# Usage: check_config_readers.sh
#
# Why this exists
# ---------------
# A config key that nothing reads is not harmless. Two of the three found at
# f189422 made an operator hand over a REAL CREDENTIAL for zero function
# (SlackConfig.app_token, LarkConfig.encrypt_key, WebhookConfig.port). The
# schema and the runtime drifted apart with nothing checking. This is the check.
#
# It began (check_channel_config_readers.sh) covering only the 15 per-channel
# configs; `agent.parallel_tools` and other dead keys sat in its blind spot.
# This generalization (plan 261) covers EVERY `*Config` struct in schema.rs.
#
# How it works
# ------------
# For each `*Config` struct in src/config/schema.rs, extract its field names and
# look for a read (`.field`) anywhere in src/ outside the schema, the config-API
# redaction module, and test code. A field with no reader is reported.
#
# LIMITS (why this is ADVISORY, not blocking, at whole-schema scale):
#   * A field read by destructuring, by-value move, or serde-only never shows a
#     `.field` hit, so it can false-positive as unread.
#   * A field name shared by two configs (`enabled`, `model`, `api_key`, ...)
#     cannot be attributed by a bare grep — reported as UNVERIFIABLE, never a
#     failure.
# The reliable fix is a syn-based AST pass (tracked as a follow-up spike). Until
# then, the current hit list is frozen as KNOWN_UNREAD (the advisory baseline)
# and only a NEW unread *unique* field fails. Wire this step continue-on-error in
# ci-run.yml; flip to blocking once the AST pass lands and KNOWN_UNREAD is empty.

set -euo pipefail

SCHEMA="src/config/schema.rs"

# field<TAB>struct<TAB>reason — the advisory baseline. Entries here are the
# grep-based hit list at the time of generalization (plan 261); many are false
# positives from the LIMITS above, some are genuinely dead (plan 257 territory).
# Do NOT hand-grow this list to silence a new real finding — fix the field.
# Regenerate deliberately only when the AST-based check replaces the grep.
KNOWN_UNREAD=$(cat <<'EOF'
sign_events	AuditConfig	audit-log signing is unimplemented; key surfaced but never read
chunk_max_tokens	MemoryConfig	plan-261 grep baseline; verify against reader before deleting
max_images	MultimodalConfig	plan-261 grep baseline; verify against reader before deleting
max_memory_mb	ResourceLimitsConfig	part of the dead [security.sandbox] layer (deep-scan plans 196-219)
max_cpu_time_seconds	ResourceLimitsConfig	part of the dead [security.sandbox] layer (deep-scan plans 196-219)
max_subprocesses	ResourceLimitsConfig	part of the dead [security.sandbox] layer (deep-scan plans 196-219)
memory_monitoring	ResourceLimitsConfig	part of the dead [security.sandbox] layer (deep-scan plans 196-219)
firejail_args	SandboxConfig	part of the dead [security.sandbox] layer (deep-scan plans 196-219)
resources	SecurityConfig	container for the dead ResourceLimitsConfig (deep-scan plans 196-219)
EOF
)

# Every `*Config` struct except the top-level container `Config` itself.
STRUCTS="$(grep -oE 'pub struct [A-Za-z0-9]*Config ' "$SCHEMA" \
  | sed -E 's/pub struct ([A-Za-z0-9]+) /\1/' \
  | grep -vxE 'Config' \
  | sort -u)"

fail=0
checked=0
ambiguous=0

# A field name used by more than one config cannot be resolved by a bare
# `.field` grep, which is exactly how SlackConfig.app_token went unnoticed. Such
# fields are reported UNVERIFIABLE rather than passed silently.
all_field_names="$(for s in $STRUCTS; do
  awk "/pub struct $s /,/^}/" "$SCHEMA" \
    | { grep -oE '^\s+pub [a-z_0-9]+:' || true; } \
    | sed -E 's/^[[:space:]]*pub //; s/://'
done)"

is_ambiguous() {
  [ "$(grep -cxF "$1" <<<"$all_field_names")" -gt 1 ]
}

for st in $STRUCTS; do
  # Trailing space before `{` bounds the name so FooConfig does not also match
  # FooConfigExtra. awk has no \b.
  fields="$(awk "/pub struct $st /,/^}/" "$SCHEMA" \
    | { grep -oE '^\s+pub [a-z_0-9]+:' || true; } \
    | sed -E 's/^[[:space:]]*pub //; s/://')"

  for f in $fields; do
    [ -n "$f" ] || continue
    checked=$((checked + 1))

    # A read is any `.field` outside the schema, the redaction module, and tests.
    hits="$(grep -rn "\.$f\b" src/ --include='*.rs' 2>/dev/null \
      | grep -v "^$SCHEMA:" \
      | grep -v 'src/gateway/config_api.rs' \
      | { grep -vE '(^|/)tests?\.rs:|#\[test\]|#\[cfg\(test\)\]' || true; } \
      | { grep -vE '_test\.rs:' || true; } \
      | head -1 || true)"

    # A listed exception is reported whether or not a hit was found — for an
    # ambiguous name the hit proves nothing, and the list is the real record.
    if grep -qP "^\Q$f\E\t\Q$st\E\t" <<<"$KNOWN_UNREAD"; then
      reason="$(grep -P "^\Q$f\E\t\Q$st\E\t" <<<"$KNOWN_UNREAD" | cut -f3-)"
      echo "known:  $st.$f — $reason"
      continue
    fi

    [ -n "$hits" ] && continue

    if is_ambiguous "$f"; then
      echo "UNVERIFIABLE: $st.$f — the name is shared with another config,"
      echo "              so a bare grep cannot tell whose reader it found."
      ambiguous=$((ambiguous + 1))
      continue
    fi

    echo "ERROR:  $st.$f is declared in the schema but read by no runtime code."
    echo "        If it is a credential, an operator is supplying one for nothing."
    echo "        Implement it, delete it, or add it to KNOWN_UNREAD with a plan."
    fail=1
  done
done

echo "checked $checked config field(s); $ambiguous unverifiable by name"

if [ "$ambiguous" -gt 0 ]; then
  echo
  echo "Note: unverifiable fields are NOT failures. They are the known limit of a"
  echo "grep-based check. Resolving them needs a reader-aware (syn/AST) check or a"
  echo "unique rename; until then KNOWN_UNREAD is the record."
fi

if [ "$fail" -ne 0 ]; then
  echo
  echo "A config field has no reader. See the message(s) above."
  exit 1
fi

exit 0
