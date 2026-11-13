#!/usr/bin/env bash
#
# The local Rust quality gate: format, clippy and the identity-string check.
#
# Usage:
#   rust_quality_gate.sh [--strict] [--skip-fmt]
#
#   --strict    clippy denies every warning, not just the correctness group
#   --skip-fmt  skip `cargo fmt --all -- --check`
#
# `--skip-fmt` exists because the CI workflow runs the format check in its
# own cheap job (`fmt`), and running it here as well would check every file
# twice. Without the flag this script is the complete local gate — it is
# what `dev/ci.sh` and `.githooks/pre-push` call — so it formats first.
#
# Shared clippy run
# -----------------
# The CI lint job lints the crate once and hands the diagnostics to both
# lint gates. When `CLIPPY_JSON_FILE` is set, this script reads that file
# instead of running clippy itself; `CLIPPY_EXIT_CODE` carries the exit code
# of that run. Both variables describe one clippy run and have to be set
# together: a JSON file without its exit code would let a clippy failure with
# no parsable diagnostic slip through, so the gate fails loudly instead of
# guessing. With `CLIPPY_JSON_FILE` unset, the script runs clippy itself,
# exactly as it always has.

set -euo pipefail

MODE="correctness"
SKIP_FMT=false

for arg in "$@"; do
    case "$arg" in
        --strict) MODE="strict" ;;
        --skip-fmt) SKIP_FMT=true ;;
        *)
            echo "rust_quality_gate.sh: unknown argument '${arg}'" >&2
            echo "usage: rust_quality_gate.sh [--strict] [--skip-fmt]" >&2
            exit 2
            ;;
    esac
done

if [ "$SKIP_FMT" = "true" ]; then
    echo "==> rust quality: cargo fmt skipped (--skip-fmt)"
else
    echo "==> rust quality: cargo fmt --all -- --check"
    cargo fmt --all -- --check
fi

# Read the diagnostics of one shared clippy run and decide what blocks.
#
# The shared run uses the flags the quality gate already used, so its
# diagnostics carry the levels that run produced. An `error`-level
# diagnostic is a compile error, or a correctness lint that no crate
# attribute lowered — the set on which clippy exits non-zero. A
# `warning`-level diagnostic is every other lint, including a correctness
# lint that a crate attribute lowered to a warning: the old run reported
# those the same way. The strict mode denies them (`-D warnings` did), the
# correctness mode does not.
json_clippy_gate() {
    local json_file="$1"
    local mode="$2"
    local clippy_exit="$3"
    local gate_status=0

    python3 - "$json_file" "$mode" "$clippy_exit" <<'PY' || gate_status=$?
import json
import sys

json_path = sys.argv[1]
mode = sys.argv[2]
try:
    clippy_exit = int(sys.argv[3])
except ValueError:
    print("The shared clippy run has no usable exit code; the gate cannot judge it.")
    sys.exit(2)

errors = []
warnings = []

with open(json_path, "r", encoding="utf-8", errors="ignore") as handle:
    for raw_line in handle:
        line = raw_line.strip()
        if not line:
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError:
            continue
        if payload.get("reason") != "compiler-message":
            continue

        message = payload.get("message") or {}
        level = message.get("level")
        if level not in ("error", "warning"):
            continue

        code_object = message.get("code") or {}
        code = code_object.get("code") if isinstance(code_object, dict) else None
        text = message.get("message") or ""
        rendered = message.get("rendered") or ""

        spans = message.get("spans") or []
        primary = [span for span in spans if span.get("is_primary")]
        if not primary:
            primary = spans
        where = ""
        if primary:
            span = primary[0]
            where = "{}:{}".format(span.get("file_name"), span.get("line_start"))

        # `rendered` is rustc's own human-readable rendering of the
        # diagnostic. It is not always present, so the message and the code
        # are the fallback.
        entry = "{} {} {}".format(where, level.upper(), code or "-")
        if text:
            entry = "{} {}".format(entry, text)
        if rendered:
            entry = rendered.rstrip("\n")

        if level == "error":
            errors.append(entry)
        else:
            warnings.append(entry)

print(
    "clippy diagnostics read from {}: {} error(s), {} warning(s)".format(
        json_path, len(errors), len(warnings)
    )
)

if errors:
    print("")
    print("Errors:")
    for entry in errors:
        print(entry)

if warnings:
    print("")
    print("Warnings:")
    for entry in warnings:
        print(entry)

blocking = list(errors)
if mode == "strict":
    blocking.extend(warnings)

if blocking:
    print("")
    print("Clippy findings that block the quality gate: {}".format(len(blocking)))
    sys.exit(1)

if clippy_exit != 0:
    print("")
    print(
        "clippy exited {} but no error-level diagnostic explains it.".format(
            clippy_exit
        )
    )
    print("This is the ICE / link-error / cargo-failure case: failing safe.")
    sys.exit(2)

print("")
print("No blocking clippy findings.")
sys.exit(0)
PY

    return "$gate_status"
}

if [ -n "${CLIPPY_JSON_FILE:-}" ]; then
    if [ -z "${CLIPPY_EXIT_CODE:-}" ]; then
        echo "rust_quality_gate.sh: CLIPPY_JSON_FILE is set but CLIPPY_EXIT_CODE is not." >&2
        echo "rust_quality_gate.sh: both variables describe one clippy run and must be set together." >&2
        exit 2
    fi
    case "${CLIPPY_EXIT_CODE:-}" in
        *[!0-9]*)
            echo "rust_quality_gate.sh: CLIPPY_EXIT_CODE must be a decimal exit code, got '${CLIPPY_EXIT_CODE}'." >&2
            exit 2
            ;;
    esac
    if [ ! -r "$CLIPPY_JSON_FILE" ]; then
        echo "rust_quality_gate.sh: cannot read the clippy JSON file '${CLIPPY_JSON_FILE}'." >&2
        exit 2
    fi

    echo "==> rust quality: reading shared clippy diagnostics from ${CLIPPY_JSON_FILE}"
    json_clippy_gate "$CLIPPY_JSON_FILE" "$MODE" "${CLIPPY_EXIT_CODE:-}"
else
    if [ "$MODE" = "strict" ]; then
        echo "==> rust quality: cargo clippy --locked --all-targets -- -D warnings"
        cargo clippy --locked --all-targets -- -D warnings
    else
        echo "==> rust quality: cargo clippy --locked --all-targets -- -D clippy::correctness"
        cargo clippy --locked --all-targets -- -D clippy::correctness
    fi
fi

# CLAUDE.md §9.1 identity gate. It is called from here, rather than as its own
# workflow step, because this effort may not edit .github/workflows/**. This
# script already runs in the PR lint job, so the check runs on pull requests —
# but only when Rust changed. Moving it to its own step alongside the other
# source-level guards would also cover docs-only PRs.
echo "==> identity strings: scripts/ci/check_identity_strings.sh"
./scripts/ci/check_identity_strings.sh --self-test
