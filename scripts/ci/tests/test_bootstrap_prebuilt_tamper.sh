#!/usr/bin/env bash
# Tests for the pre-built binary install path of scripts/bootstrap.sh.
#
# Invariant under test: an archive that cannot be verified (the SHA256 in
# SHA256SUMS does not match the archive, or SHA256SUMS carries no entry for
# it) stops the install with its own refusal as the last message, whatever
# the flags say. An archive that is merely unavailable (no asset for this
# platform, download failed) keeps today's behaviour: "No pre-built binary
# available for this platform." under --prebuilt-only, a warning plus a
# source build otherwise.
#
# The test drives the installer's functions, so scripts/bootstrap.sh has to
# be sourceable without running the install. That guard is asserted first
# (case 1); a fake `cargo` on PATH keeps a regression from starting a real
# source build while the guard is missing.
#
# The release artifacts are served over file:// from a throwaway directory,
# so the test needs no network and no Rust toolchain.
#
# Usage:
#   bash scripts/ci/tests/test_bootstrap_prebuilt_tamper.sh
#
# Exit code:
#   0 on PASS
#   1 on FAIL

set -uo pipefail

# Anchor on script location so the test works from any cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
BOOTSTRAP="$REPO_DIR/scripts/bootstrap.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

PASS=0
FAIL=0

log_pass() {
    printf '%b\n' "${GREEN}PASS${NC} $1"
    PASS=$((PASS + 1))
}

log_fail() {
    printf '%b\n' "${RED}FAIL${NC} $1"
    FAIL=$((FAIL + 1))
}

# --- preflight ---------------------------------------------------------------

if [ ! -f "$BOOTSTRAP" ]; then
    printf '%s\n' "installer not found at $BOOTSTRAP" >&2
    exit 1
fi

for cmd in curl tar; do
    if ! command -v "$cmd" >/dev/null 2>&1; then
        printf '%s\n' "$cmd is required to test the pre-built install path" >&2
        exit 1
    fi
done

if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
    printf '%s\n' "sha256sum or shasum is required to build the test fixtures" >&2
    exit 1
fi

# --- load the installer ------------------------------------------------------

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

# A fake cargo: only a safety net for the case where the sourceable guard
# is missing and the installer would otherwise start a real source build.
mkdir -p "$WORK_DIR/fakebin"
cat > "$WORK_DIR/fakebin/cargo" <<'FAKE_CARGO'
#!/usr/bin/env bash
printf 'fake cargo: %s\n' "$*"
exit 0
FAKE_CARGO
chmod +x "$WORK_DIR/fakebin/cargo"

SOURCE_LOG="$WORK_DIR/source.log"
SAVED_PATH="$PATH"
PATH="$WORK_DIR/fakebin:$PATH"
RANTAICLAW_RELEASE_BASE_URL="file://$WORK_DIR/no-such-release"
RANTAICLAW_INSTALL_DIR="$WORK_DIR/install"
export RANTAICLAW_RELEASE_BASE_URL RANTAICLAW_INSTALL_DIR
# shellcheck source=../../bootstrap.sh
source "$BOOTSTRAP" > "$SOURCE_LOG" 2>&1
SOURCE_STATUS=$?
PATH="$SAVED_PATH"
unset RANTAICLAW_RELEASE_BASE_URL RANTAICLAW_INSTALL_DIR
printf 'SOURCE-PROBE-FINISHED\n' >> "$SOURCE_LOG"

# The installer computes its step counter when it runs. A sourced copy has to
# do that itself, or the [N/T] labels have no denominator.
compute_step_total

# --- helpers -----------------------------------------------------------------

# Build a release directory that can be served over file:// .
# $1 = directory to fill
# $2 = good | tampered | no-entry | empty
make_release_dir() {
    local dir="$1"
    local mode="$2"
    local target work archive sum

    mkdir -p "$dir"
    if [ "$mode" = "empty" ]; then
        return 0
    fi

    target="$(detect_release_target)" || return 1
    work="$dir/work"
    mkdir -p "$work"
    printf '#!/bin/sh\nprintf "rantaiclaw 0.0.0-test\\n"\n' > "$work/rantaiclaw"
    chmod +x "$work/rantaiclaw"
    archive="$dir/rantaiclaw-${target}.tar.gz"
    tar -czf "$archive" -C "$work" rantaiclaw
    sum="$(sha256_compute "$archive")"

    case "$mode" in
        good)
            printf '%s  %s\n' "$sum" "rantaiclaw-${target}.tar.gz" > "$dir/SHA256SUMS"
            ;;
        tampered)
            printf '%s  %s\n' \
                "0000000000000000000000000000000000000000000000000000000000000000" \
                "rantaiclaw-${target}.tar.gz" > "$dir/SHA256SUMS"
            ;;
        no-entry)
            printf '%s  %s\n' "$sum" "some-other-asset.tar.gz" > "$dir/SHA256SUMS"
            ;;
        *)
            return 1
            ;;
    esac
}

# Run a case body in a subshell that inherits the sourced installer.
# $1 = body function name, $2 = case directory, $3 = output file
# Sets CASE_STATUS to the exit status of the body.
run_case() {
    local body="$1"
    local dir="$2"
    local out="$3"

    CASE_STATUS=0
    (
        set -uo pipefail
        RANTAICLAW_RELEASE_BASE_URL="file://$dir/release"
        RANTAICLAW_INSTALL_DIR="$dir/install"
        export RANTAICLAW_RELEASE_BASE_URL RANTAICLAW_INSTALL_DIR
        "$body" "$dir"
    ) > "$out" 2>&1 || CASE_STATUS=$?
}

# The refusal has to be the last thing the installer says.
assert_refusal_is_last() {
    local out="$1"
    local label="$2"
    local last

    last="$(grep -v '^[[:space:]]*$' "$out" | tail -n 1)"
    if printf '%s' "$last" | grep -q 'Refusing to install a tampered or corrupt archive\.'; then
        log_pass "$label"
    else
        log_fail "$label (last message was: '$last')"
    fi
}

assert_output_contains() {
    local out="$1"
    local needle="$2"
    local label="$3"

    if grep -qF "$needle" "$out"; then
        log_pass "$label"
    else
        log_fail "$label (missing: '$needle')"
    fi
}

assert_output_lacks() {
    local out="$1"
    local needle="$2"
    local label="$3"

    if grep -qF "$needle" "$out"; then
        log_fail "$label (found: '$needle')"
    else
        log_pass "$label"
    fi
}

assert_status() {
    local actual="$1"
    local expected="$2"
    local label="$3"

    if [ "$actual" -eq "$expected" ]; then
        log_pass "$label"
    else
        log_fail "$label (expected $expected, got $actual)"
    fi
}

assert_not_installed() {
    local install_dir="$1"
    local label="$2"

    if [ ! -e "$install_dir/rantaiclaw" ]; then
        log_pass "$label"
    else
        log_fail "$label (found $install_dir/rantaiclaw)"
    fi
}

assert_state() {
    local state_file="$1"
    local key="$2"
    local expected="$3"
    local label="$4"
    local actual

    if [ ! -f "$state_file" ]; then
        log_fail "$label (no state file)"
        return
    fi
    actual="$(grep "^${key}=" "$state_file" | head -1 | cut -d= -f2-)"
    if [ "$actual" = "$expected" ]; then
        log_pass "$label"
    else
        log_fail "$label (expected $expected, got '$actual')"
    fi
}

# --- case bodies -------------------------------------------------------------
# Each body runs inside the subshell of run_case and inherits the sourced
# installer. State that the assertions need goes into $1/state, never into
# the output, so the "the refusal is the last message" check stays exact.

body_install_prebuilt() {
    install_prebuilt_binary
}

body_caller_default_flags() {
    local status=0
    install_prebuilt_or_fallback || status=$?
    printf 'FORCE_SOURCE_BUILD=%s\n' "${FORCE_SOURCE_BUILD}" > "$1/state"
    return "$status"
}

body_caller_prebuilt_only() {
    local status=0
    PREBUILT_ONLY=true
    install_prebuilt_or_fallback || status=$?
    printf 'FORCE_SOURCE_BUILD=%s\n' "${FORCE_SOURCE_BUILD}" > "$1/state"
    return "$status"
}

body_caller_from_source() {
    local status=0
    FORCE_SOURCE_BUILD=true
    PREFER_PREBUILT=false
    install_prebuilt_or_fallback || status=$?
    printf 'FORCE_SOURCE_BUILD=%s\n' "${FORCE_SOURCE_BUILD}" > "$1/state"
    return "$status"
}

# --- case 1: the installer is sourceable --------------------------------------

printf '\n=== the installer is sourceable without running the install ===\n'

if [ "$SOURCE_STATUS" -ne 0 ]; then
    log_fail "sourcing scripts/bootstrap.sh returns successfully (status $SOURCE_STATUS)"
else
    log_pass "sourcing scripts/bootstrap.sh returns successfully"
fi

if grep -q 'SOURCE-PROBE-FINISHED' "$SOURCE_LOG"; then
    log_pass "sourcing scripts/bootstrap.sh returns to the caller"
else
    log_fail "sourcing scripts/bootstrap.sh returns to the caller (probe did not finish)"
fi

if grep -q 'RantaiClaw Installer' "$SOURCE_LOG"; then
    log_fail "sourcing scripts/bootstrap.sh does not run the installer (banner found)"
else
    log_pass "sourcing scripts/bootstrap.sh does not run the installer"
fi

if declare -F install_prebuilt_binary >/dev/null 2>&1; then
    log_pass "the sourced installer exposes install_prebuilt_binary"
else
    log_fail "the sourced installer exposes install_prebuilt_binary (not defined)"
fi

if declare -F install_prebuilt_or_fallback >/dev/null 2>&1; then
    log_pass "the sourced installer exposes install_prebuilt_or_fallback"
else
    log_fail "the sourced installer exposes install_prebuilt_or_fallback (not defined)"
fi

if grep -q 'RantaiClaw Installer' "$SOURCE_LOG" || ! declare -F install_prebuilt_binary >/dev/null 2>&1; then
    printf '\n=== summary ===\n'
    printf 'Result: %d passed, %d failed\n' "$PASS" "$FAIL"
    printf '%s\n' "scripts/bootstrap.sh is not sourceable; the remaining cases cannot run" >&2
    exit 1
fi

# --- case 2: a verified archive installs --------------------------------------

printf '\n=== a verified archive installs ===\n'

CASE_DIR="$(mktemp -d)"
make_release_dir "$CASE_DIR/release" good
run_case body_install_prebuilt "$CASE_DIR" "$CASE_DIR/output"
assert_status "$CASE_STATUS" 0 "a matching SHA256SUMS entry installs the archive"
if [ -x "$CASE_DIR/install/rantaiclaw" ]; then
    log_pass "the verified archive lands in the install directory"
else
    log_fail "the verified archive lands in the install directory (not found)"
fi
rm -rf "$CASE_DIR"

# --- case 3: a tampered archive stops the install ----------------------------

printf '\n=== a tampered archive stops the install ===\n'

CASE_DIR="$(mktemp -d)"
make_release_dir "$CASE_DIR/release" tampered
run_case body_install_prebuilt "$CASE_DIR" "$CASE_DIR/output"
assert_status "$CASE_STATUS" 2 "a checksum mismatch returns the tamper status (2)"
assert_refusal_is_last "$CASE_DIR/output" "the refusal is the last message of a tampered archive"
assert_not_installed "$CASE_DIR/install" "a tampered archive installs nothing"
rm -rf "$CASE_DIR"

# --- case 4: an archive SHA256SUMS does not cover stops the install -----------

printf '\n=== an archive without a SHA256SUMS entry stops the install ===\n'

CASE_DIR="$(mktemp -d)"
make_release_dir "$CASE_DIR/release" no-entry
run_case body_install_prebuilt "$CASE_DIR" "$CASE_DIR/output"
assert_status "$CASE_STATUS" 2 "a missing SHA256SUMS entry returns the tamper status (2)"
assert_refusal_is_last "$CASE_DIR/output" "the refusal is the last message of an uncovered archive"
assert_not_installed "$CASE_DIR/install" "an uncovered archive installs nothing"
rm -rf "$CASE_DIR"

# --- case 5: a failed download stays "unavailable" ----------------------------

printf '\n=== a failed download stays unavailable ===\n'

CASE_DIR="$(mktemp -d)"
make_release_dir "$CASE_DIR/release" empty
run_case body_install_prebuilt "$CASE_DIR" "$CASE_DIR/output"
assert_status "$CASE_STATUS" 1 "a failed download returns the unavailable status (1)"
assert_output_contains "$CASE_DIR/output" "Could not download release asset:" \
    "a failed download keeps today's message"
assert_output_lacks "$CASE_DIR/output" "Refusing to install a tampered or corrupt archive." \
    "a failed download is not a tamper refusal"
rm -rf "$CASE_DIR"

# --- case 6: no asset for this platform stays "unavailable" -------------------

printf '\n=== no asset for this platform stays unavailable ===\n'

CASE_DIR="$(mktemp -d)"
mkdir -p "$CASE_DIR/fakebin"
cat > "$CASE_DIR/fakebin/uname" <<'FAKE_UNAME'
#!/usr/bin/env bash
case "${1:-}" in
    -s) printf 'Linux\n' ;;
    -m) printf 'riscv64\n' ;;
    *) printf 'Linux\n' ;;
esac
FAKE_UNAME
chmod +x "$CASE_DIR/fakebin/uname"
make_release_dir "$CASE_DIR/release" good
(
    set -uo pipefail
    PATH="$CASE_DIR/fakebin:$PATH"
    export PATH
    install_prebuilt_binary
) > "$CASE_DIR/output" 2>&1
CASE_STATUS=$?
assert_status "$CASE_STATUS" 1 "an unsupported platform returns the unavailable status (1)"
assert_output_contains "$CASE_DIR/output" "No pre-built binary available for Linux/riscv64." \
    "an unsupported platform keeps today's message"
rm -rf "$CASE_DIR"

# --- case 7: the caller stops on a tampered archive ---------------------------

printf '\n=== the caller stops on a tampered archive ===\n'

for flags in default prebuilt_only; do
    CASE_DIR="$(mktemp -d)"
    make_release_dir "$CASE_DIR/release" tampered
    if [ "$flags" = "prebuilt_only" ]; then
        run_case body_caller_prebuilt_only "$CASE_DIR" "$CASE_DIR/output"
    else
        run_case body_caller_default_flags "$CASE_DIR" "$CASE_DIR/output"
    fi
    assert_status "$CASE_STATUS" 2 "the caller returns the tamper status (2) with $flags flags"
    assert_refusal_is_last "$CASE_DIR/output" "the refusal is the last message with $flags flags"
    assert_output_lacks "$CASE_DIR/output" "No pre-built binary available for this platform." \
        "a tamper is not reported as an unavailable binary ($flags flags)"
    assert_output_lacks "$CASE_DIR/output" "falling back to source build" \
        "a tamper does not fall back to a source build ($flags flags)"
    assert_state "$CASE_DIR/state" "FORCE_SOURCE_BUILD" "false" \
        "a tamper leaves the source build off ($flags flags)"
    assert_not_installed "$CASE_DIR/install" "a tampered archive installs nothing ($flags flags)"
    rm -rf "$CASE_DIR"
done

# --- case 8: the caller keeps the current behaviour for an unavailable archive --

printf '\n=== the caller keeps the current behaviour for an unavailable archive ===\n'

CASE_DIR="$(mktemp -d)"
make_release_dir "$CASE_DIR/release" empty
run_case body_caller_prebuilt_only "$CASE_DIR" "$CASE_DIR/output"
assert_status "$CASE_STATUS" 1 "the caller stops under --prebuilt-only when nothing is available"
assert_output_contains "$CASE_DIR/output" "No pre-built binary available for this platform." \
    "--prebuilt-only keeps today's message"
assert_output_contains "$CASE_DIR/output" \
    "Re-run with --from-source on a machine with a Rust toolchain to build locally." \
    "--prebuilt-only keeps today's hint"
rm -rf "$CASE_DIR"

CASE_DIR="$(mktemp -d)"
make_release_dir "$CASE_DIR/release" empty
run_case body_caller_default_flags "$CASE_DIR" "$CASE_DIR/output"
assert_status "$CASE_STATUS" 0 "the caller continues when nothing is available"
assert_output_contains "$CASE_DIR/output" "Binary install unavailable; falling back to source build." \
    "the default path keeps today's warning"
assert_state "$CASE_DIR/state" "FORCE_SOURCE_BUILD" "true" \
    "the default path still falls back to a source build"
rm -rf "$CASE_DIR"

# --- case 9: --from-source skips the pre-built path --------------------------

printf '\n=== --from-source skips the pre-built path ===\n'

CASE_DIR="$(mktemp -d)"
make_release_dir "$CASE_DIR/release" tampered
run_case body_caller_from_source "$CASE_DIR" "$CASE_DIR/output"
assert_status "$CASE_STATUS" 0 "--from-source continues the install"
assert_output_lacks "$CASE_DIR/output" "Installing pre-built binary" \
    "--from-source never reaches the pre-built install"
assert_output_lacks "$CASE_DIR/output" "Refusing to install a tampered or corrupt archive." \
    "--from-source never verifies a release archive"
rm -rf "$CASE_DIR"

# --- summary -----------------------------------------------------------------

printf '\n=== summary ===\n'
printf 'Result: %d passed, %d failed\n' "$PASS" "$FAIL"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
exit 0
