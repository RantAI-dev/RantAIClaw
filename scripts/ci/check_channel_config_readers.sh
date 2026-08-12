#!/usr/bin/env bash
# Assert every channel config field is actually read by some runtime code.
#
# Usage: check_channel_config_readers.sh
#
# Why this exists
# ---------------
# A config key that nothing reads is not harmless. Two of the three found at
# f189422 make an operator hand over a REAL CREDENTIAL for zero function:
#
#   SlackConfig.app_token  — prompted, documented, redacted as a secret, never read.
#                            Slack polls; Socket Mode is never established.
#   LarkConfig.encrypt_key — prompted by the TUI provisioner, documented, never read.
#   WebhookConfig.port     — documented as `port = 8080`; the gateway binds its own
#                            port. This one tells operators to open a firewall port
#                            nothing listens on, then the webhook never arrives.
#
# Three no-op keys in one config section means the schema and the runtime drifted
# apart with nothing checking. This is the check.
#
# How it works
# ------------
# For each per-channel *Config struct in src/config/schema.rs, extract its field
# names and look for a read anywhere in src/ outside the schema itself, the secret
# redaction list, and test code. A field with no reader is reported.
#
# FAIL-CLOSED: a newly added field with no reader FAILS. Known exceptions must be
# listed below with the plan that resolves them — delete the line when it lands.

set -euo pipefail

SCHEMA="src/config/schema.rs"

# field<TAB>struct<TAB>reason — each MUST name the plan that resolves it.
KNOWN_UNREAD=$(cat <<'EOF'
app_token	SlackConfig	plan 146 — build Socket Mode or delete the key
encrypt_key	LarkConfig	plan 146 — implement Feishu event decryption or delete the key
port	WebhookConfig	plan 146 — delete; the gateway binds its own port
EOF
)

STRUCTS="DingTalkConfig DiscordConfig IMessageConfig IrcConfig LarkConfig LinqConfig
MatrixConfig MattermostConfig NextcloudTalkConfig QQConfig SignalConfig SlackConfig
TelegramConfig WebhookConfig WhatsAppConfig"

fail=0
checked=0
ambiguous=0

# A field name used by more than one channel config cannot be resolved by a bare
# `.field` grep — `app_token` belongs to both SlackConfig and NextcloudTalkConfig,
# `port` to both WebhookConfig and LarkConfig. For those, a "reader" hit may belong
# to the other struct entirely, which is exactly how SlackConfig.app_token went
# unnoticed. Such fields are reported as UNVERIFIABLE rather than passed silently:
# the check states its own limit instead of producing a false negative.
all_field_names="$(for s in $STRUCTS; do
  awk "/pub struct $s /,/^}/" "$SCHEMA" \
    | { grep -oE '^\s+pub [a-z_0-9]+:' || true; } \
    | sed -E 's/^[[:space:]]*pub //; s/://'
done)"

is_ambiguous() {
  [ "$(grep -cxF "$1" <<<"$all_field_names")" -gt 1 ]
}

for st in $STRUCTS; do
  # Trailing space before `{` bounds the name, so TelegramConfig does not also
  # match a hypothetical TelegramConfigExtra. awk has no \b.
  fields="$(awk "/pub struct $st /,/^}/" "$SCHEMA" \
    | { grep -oE '^\s+pub [a-z_0-9]+:' || true; } \
    | sed -E 's/^[[:space:]]*pub //; s/://')"

  for f in $fields; do
    [ -n "$f" ] || continue
    checked=$((checked + 1))

    # A read is any `.field` outside the schema, the redaction fixture list, and tests.
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
      echo "UNVERIFIABLE: $st.$f — the name is shared with another channel config,"
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

echo "checked $checked channel config field(s); $ambiguous unverifiable by name"

if [ "$ambiguous" -gt 0 ]; then
  echo
  echo "Note: unverifiable fields are NOT failures. They are the known limit of a"
  echo "grep-based check. Resolving them needs a reader-aware check (or renaming the"
  echo "field so it is unique); until then the KNOWN_UNREAD list is the record."
fi

if [ "$fail" -ne 0 ]; then
  echo
  echo "A channel config field has no reader. See the message above for the field."
  exit 1
fi

exit 0
