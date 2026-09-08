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
#   >35MB  — hard error (safeguard)
#   >30MB  — warning (advisory: this target is close to the cap)
#    5MB   — the aspiration, reported but NOT warned on. See below.
#
# Why 5MB stopped being a warning
# -------------------------------
# It fired on every target of every release — six warnings, six releases in a
# row, none of them actionable — and a warning that always fires is training,
# not information. The number is kept as the stated aspiration because deleting
# it would erase the goal along with the noise; it is printed, not raised.
#
# Measured from the published v0.30.0-alpha artefacts on 2026-09-08, by
# downloading all six and reading the extracted binary:
#
#   x86_64-unknown-linux-gnu       36,159,376 B   34.48 MiB   ← 540,784 B under the cap
#   x86_64-apple-darwin            30,118,616 B   28.72 MiB
#   x86_64-pc-windows-msvc         29,429,760 B   28.07 MiB
#   aarch64-unknown-linux-gnu      28,295,856 B   26.99 MiB
#   armv7-unknown-linux-gnueabihf  26,862,132 B   25.62 MiB
#   aarch64-apple-darwin           21,476,752 B   20.48 MiB
#
# So the 30MB advisory fires on exactly ONE of the six, which makes it a signal
# rather than wolf-crying: x86_64-linux has **0.52 MiB of headroom** to the hard
# cap. Anyone raising the floor again should start from that number, and should
# know it is one target's problem and not the platform's — the same build is
# 14 MiB clear of the cap on aarch64-darwin.
#
# Floor history:
#   v0.6.39 → rig-core multi-provider adapter became default streaming
#             path (+5MB). Safeguard 20→25, advisory 15→20.
#   v0.6.49 → whatsapp-web (wa-rs + transport + protobuf) moved into
#             default features (+1.4MB). Safeguard 25→30, advisory
#             20→25. WhatsApp Web is the second-largest messaging
#             platform globally; gating it behind a build flag broke
#             the "complete but still light" thesis.
#   v0.6.76 → kb (sqlite-vec vector store + lopdf/pdf-extract ingestion)
#             moved into default features (+~5MB → ~31MB). Safeguard
#             30→35, advisory 25→30. A knowledge base is core to the
#             agent's usefulness out of the box; gating `rantaiclaw kb`
#             behind a build flag meant shipped binaries had no KB at all.
#
# This gate measures the SHIPPED profile, `release-fast` — opt-level=z,
# lto=fat, strip=true, panic=abort, and codegen-units=**8**. It previously
# described `codegen-units=1`, which is `[profile.release]`: the profile the
# release workflow does not build. As of 2026-09-07 the Dockerfile builds
# `release-fast` too, so one profile ships everywhere. The 5MB target is kept
# aspirational; safeguard/advisory get raised one tier when the floor
# moves so the gate stays honest without silently disabling.
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

if [ "$SIZE" -gt 36700160 ]; then
  echo "::error::Binary exceeds 35MB safeguard (${SIZE_MB}MB)"
  exit 1
elif [ "$SIZE" -gt 31457280 ]; then
  echo "::warning::Binary exceeds 30MB advisory target (${SIZE_MB}MB)"
elif [ "$SIZE" -gt 5242880 ]; then
  # Reported, not warned. See the threshold notes at the top: this fired on
  # every target of every release and taught readers to skip the size step.
  echo "Above the 5MB aspiration (${SIZE_MB}MB); under the 30MB advisory."
else
  echo "Binary size within target."
fi
