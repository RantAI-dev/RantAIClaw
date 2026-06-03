#!/usr/bin/env bash
# Phase-12 binary-size regression guard.
#
# Builds the rantaiclaw release binary twice — without the `kb` feature
# (baseline) and with it — then asserts the delta stays under the 2 MB
# budget. Run this from anywhere; the script anchors paths against its
# own location so CI invocations don't need a specific cwd.
#
# Usage:
#   bash tests/kb/size_test.sh
#
# Exit code:
#   0 on PASS (delta < 2 MB)
#   1 on FAIL (delta >= 2 MB)
#   2 on build failure
#
# CI wiring: add this to .github/workflows/<...>.yml as a step that
# only runs on PRs touching `src/kb/**` or `Cargo.toml`. Local
# pre-merge invocation is recommended for any KB-touching commit.

set -euo pipefail

# Anchor on script location → walk up to the rantaiclaw crate root
# (`tests/kb/size_test.sh` → `..` → `tests` → `..` → crate root).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
cd "$CRATE_ROOT"

BUDGET_BYTES=$((2 * 1024 * 1024))
BINARY_PATH="target/release/rantaiclaw"

echo "[size_test] crate root: $CRATE_ROOT"
echo "[size_test] building baseline (no kb feature)..."
if ! cargo build --release --quiet 2>&1; then
    echo "[size_test] baseline build FAILED" >&2
    exit 2
fi
baseline_bytes=$(stat -c%s "$BINARY_PATH" 2>/dev/null || stat -f%z "$BINARY_PATH")

echo "[size_test] building with kb feature..."
if ! cargo build --release --features kb --quiet 2>&1; then
    echo "[size_test] kb-feature build FAILED" >&2
    exit 2
fi
with_kb_bytes=$(stat -c%s "$BINARY_PATH" 2>/dev/null || stat -f%z "$BINARY_PATH")

delta=$((with_kb_bytes - baseline_bytes))
delta_mb=$(awk "BEGIN {printf \"%.2f\", $delta / 1024 / 1024}")
baseline_mb=$(awk "BEGIN {printf \"%.2f\", $baseline_bytes / 1024 / 1024}")
with_kb_mb=$(awk "BEGIN {printf \"%.2f\", $with_kb_bytes / 1024 / 1024}")
budget_mb=$(awk "BEGIN {printf \"%.2f\", $BUDGET_BYTES / 1024 / 1024}")

echo ""
echo "[size_test] baseline:   $baseline_bytes bytes ($baseline_mb MB)"
echo "[size_test] with kb:    $with_kb_bytes bytes ($with_kb_mb MB)"
echo "[size_test] delta:      $delta bytes ($delta_mb MB)"
echo "[size_test] budget:     $BUDGET_BYTES bytes ($budget_mb MB)"

if [ "$delta" -gt "$BUDGET_BYTES" ]; then
    echo "[size_test] FAIL: kb feature adds $delta_mb MB, over the $budget_mb MB budget" >&2
    echo "[size_test] audit suggestion: cargo bloat --release --features kb -n 20" >&2
    exit 1
fi

echo "[size_test] PASS"
