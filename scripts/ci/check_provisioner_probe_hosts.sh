#!/usr/bin/env bash
# Assert every channel provisioner probes the same host its channel module talks to.
#
# Usage: check_provisioner_probe_hosts.sh
#
# Why this exists
# ---------------
# A provisioner validates an operator's credential by calling the platform's API.
# If it calls the WRONG host, the operator's live credential is transmitted to a
# domain this project does not control — and the probe can never succeed, so the
# setup wizard warns "token may be invalid" for a valid token and operators learn
# to ignore the warning.
#
# This class has already shipped twice:
#   v0.16.1-alpha — setup sent API keys to two unregistered domains. Fixed, but
#                   nothing was added to stop it recurring.
#   f189422       — `linq.rs` probes api.linq.com while the runtime uses
#                   api.linqapp.com. Found by audit, not by CI.
#
# The one-line fix is cheap. This check is what stops a third occurrence.
#
# How it works
# ------------
# For each provisioner under src/onboard/provision/channels/, extract the hosts it
# probes and assert each one appears among the hosts its matching channel module
# under src/channels/ uses. Source-level on purpose: it fails closed when a NEW
# provisioner appears with a host its channel never contacts, which a hand-maintained
# table of expectations would not.
#
# Known exceptions live in the KNOWN_MISMATCHES list below, each with the plan that
# removes it. Landing a fix means deleting its line here.

set -euo pipefail

PROV_DIR="src/onboard/provision/channels"
CHAN_DIR="src/channels"

# host<TAB>provisioner — each MUST name the plan that resolves it.
# Delete a line when its plan lands; the check then enforces it for real.
KNOWN_MISMATCHES=$(cat <<'EOF'
api.linq.com	linq	plan 132 — provisioner probes api.linq.com, runtime uses api.linqapp.com
EOF
)

fail=0
checked=0

# Hosts a provisioner is allowed to contact regardless of its channel module:
# operator-supplied server URLs (self-hosted platforms) have no fixed host.
SELF_HOSTED="matrix mattermost nextcloud_talk irc email signal"

# Emit the distinct hosts a file contacts. Never fails: a file with no URLs is a
# normal case, not an error, so every grep is guarded against an empty match.
hosts_in() {
  { grep -ohE 'https://[a-zA-Z0-9._-]+' "$1" 2>/dev/null || true; } \
    | sed -E 's#https://##' \
    | { grep -vE '^(example\.com|example\.test|x\.io|matrix\.org)$' || true; } \
    | sort -u
}

for prov in "$PROV_DIR"/*.rs; do
  name="$(basename "$prov" .rs)"
  [ "$name" = "mod" ] && continue

  # Operator-supplied server URLs have no fixed host — nothing to compare against.
  case " $SELF_HOSTED " in *" $name "*) continue ;; esac

  # Provisioner name -> channel module, where they differ.
  chan_name="$name"
  [ "$name" = "whatsapp_cloud" ] && chan_name="whatsapp"
  [ "$name" = "email" ] && chan_name="email_channel"
  chan="$CHAN_DIR/$chan_name.rs"

  [ -f "$chan" ] || { echo "note: $name has no channel module at $chan — skipped"; continue; }

  chan_hosts="$(hosts_in "$chan")"
  [ -n "$chan_hosts" ] || continue

  while IFS= read -r host; do
    [ -n "$host" ] || continue
    checked=$((checked + 1))
    if grep -qxF "$host" <<<"$chan_hosts"; then
      continue
    fi
    if grep -qP "^\Q$host\E\t\Q$name\E\t" <<<"$KNOWN_MISMATCHES"; then
      reason="$(grep -P "^\Q$host\E\t\Q$name\E\t" <<<"$KNOWN_MISMATCHES" | cut -f3-)"
      echo "known:  $name probes $host — $reason"
      continue
    fi
    echo "ERROR:  $name probes $host, but $chan never contacts it."
    echo "        Either the provisioner has the wrong host, or the channel changed."
    echo "        A credential is sent to this host during setup — do not ignore this."
    fail=1
  done <<<"$(hosts_in "$prov")"
done

echo "checked $checked provisioner probe host(s)"

if [ "$fail" -ne 0 ]; then
  echo
  echo "A provisioner sends an operator credential to a host its channel does not use."
  exit 1
fi

exit 0
