#!/usr/bin/env bash
#
# Run clippy once for the lint job and keep its JSON diagnostics.
#
# Usage:
#   run_clippy_json.sh <output-json-path>
#
# Why this exists
# ---------------
# The lint job ran clippy twice: `rust_quality_gate.sh` linted the crate at
# the correctness level, and `rust_strict_delta_gate.sh` linted it again at
# the strict level, only to read the findings on the changed lines. The second
# run re-checked every target for findings the first run had already produced.
# This script runs clippy exactly once and writes the JSON stream that both
# gates read.
#
# The lint levels are the ones the quality gate already used, so this run
# reports exactly the diagnostics that run reported: `-D clippy::correctness`
# denies the correctness group and leaves every other lint at `warning`.
#
# A lint level set by an attribute in the source wins over the level on the
# command line, so this crate's `#![warn(clippy::all, clippy::pedantic)]`
# keeps a correctness lint at `warning` — in the old quality-gate run just as
# much as in this one. The flags are the same on purpose: the gates have to
# decide on the same diagnostics they decide on today.
#
# Output (GitHub Actions output format, on stdout):
#
#   path=<output-json-path>
#   exit_code=<clippy's exit code>
#
# Exit code
# ---------
# 0 whenever clippy ran, whatever clippy's own exit code was: the gates
# decide whether a finding blocks, not this script. A non-zero exit here is
# an infrastructure failure — clippy could not be started, or the JSON file
# could not be written — and no gate can interpret that, so it stops the job
# instead of leaving the crate looking clean.
#
# The script writes the output file it was given and one temporary stderr
# file, which it removes on exit. It does not touch the tree.

set -euo pipefail

if [ "$#" -ne 1 ] || [ -z "${1:-}" ]; then
    echo "usage: run_clippy_json.sh <output-json-path>" >&2
    exit 2
fi

OUTPUT_JSON="$1"

if ! command -v cargo >/dev/null 2>&1; then
    echo "run_clippy_json.sh: cargo is not on PATH; cannot run clippy." >&2
    exit 1
fi

STDERR_FILE="$(mktemp)"
trap 'rm -f "$STDERR_FILE"' EXIT

set +e
cargo clippy --locked --all-targets --keep-going --message-format=json -- -D clippy::correctness >"$OUTPUT_JSON" 2>"$STDERR_FILE"
CLIPPY_EXIT=$?
set -e

# cargo's progress output ("Compiling ...", "Checking ...") is on stderr and
# the JSON diagnostics are on stdout. The stderr file is temporary, so its
# content is forwarded to the real stderr before the file is removed.
if [ -s "$STDERR_FILE" ]; then
    cat "$STDERR_FILE" >&2
fi

if [ "$CLIPPY_EXIT" -eq 127 ]; then
    echo "run_clippy_json.sh: cargo could not be executed." >&2
    exit 1
fi

if [ ! -s "$OUTPUT_JSON" ]; then
    echo "run_clippy_json.sh: no clippy diagnostics at $OUTPUT_JSON (clippy exit ${CLIPPY_EXIT})." >&2
    exit 1
fi

echo "path=${OUTPUT_JSON}"
echo "exit_code=${CLIPPY_EXIT}"
