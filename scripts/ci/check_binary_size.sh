#!/usr/bin/env bash
# Check binary file size against safeguard thresholds.
#
# Usage: check_binary_size.sh <binary_path> [label]
#
# Arguments:
#   binary_path  Path to the binary to check (required)
#   label        Optional label for step summary (e.g. target triple)
#
# Thresholds:
#   >25MB  — hard error (safeguard)
#   >20MB  — warning (advisory)
#   >5MB   — warning (target)
#
# v0.6.39 raised the structural floor by ~5MB when the rig-core
# multi-provider adapter became the default streaming path. The
# release profile is already maximally tuned (opt-level=z, lto=fat,
# strip=true, panic=abort, codegen-units=1) so the only knobs left
# are feature flags or dropping providers. The 5MB target is kept
# aspirational; the safeguard and advisory were raised one tier each
# to acknowledge the new baseline without silently disabling the gate.
#
# Writes to GITHUB_STEP_SUMMARY when the variable is set and label is provided.

set -euo pipefail

BIN="${1:?Usage: check_binary_size.sh <binary_path> [label]}"
LABEL="${2:-}"

if [ ! -f "$BIN" ]; then
  echo "::error::Binary not found at $BIN"
  exit 1
fi

# macOS stat uses -f%z, Linux stat uses -c%s
SIZE=$(stat -f%z "$BIN" 2>/dev/null || stat -c%s "$BIN")
SIZE_MB=$((SIZE / 1024 / 1024))
echo "Binary size: ${SIZE_MB}MB ($SIZE bytes)"

if [ -n "$LABEL" ] && [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  echo "### Binary Size: $LABEL" >> "$GITHUB_STEP_SUMMARY"
  echo "- Size: ${SIZE_MB}MB ($SIZE bytes)" >> "$GITHUB_STEP_SUMMARY"
fi

if [ "$SIZE" -gt 26214400 ]; then
  echo "::error::Binary exceeds 25MB safeguard (${SIZE_MB}MB)"
  exit 1
elif [ "$SIZE" -gt 20971520 ]; then
  echo "::warning::Binary exceeds 20MB advisory target (${SIZE_MB}MB)"
elif [ "$SIZE" -gt 5242880 ]; then
  echo "::warning::Binary exceeds 5MB target (${SIZE_MB}MB)"
else
  echo "Binary size within target."
fi
