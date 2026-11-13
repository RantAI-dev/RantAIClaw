#!/usr/bin/env bash
# Decide whether a release image may take the `:latest` Docker tag.
#
# Usage: scripts/ci/release_latest_tag.sh <release_tag> < existing-tags.txt
#        scripts/ci/release_latest_tag.sh --self-test
#
# Reads the existing `v*` tags of the repository on stdin, one per line. Prints
# `latest` when <release_tag> is the highest tag by version order (the release tag
# is added to the list, so it does not need to appear on stdin), and `skip`
# otherwise. When it prints `skip`, the reason goes to stderr. A <release_tag> that
# is not `vX.Y.Z[-suffix]` makes the script exit 2 and print nothing on stdout.
#
# Why this exists
# ---------------
# `:latest` names the most recent release. `pub-release.yml` is dispatchable for any
# tag that is an ancestor of `main`, so retrying the image push of an older release
# (v0.34.0-alpha after v0.35.0-alpha shipped) used to move `:latest` back. The
# `Compute Docker release tags` step now asks this script and adds `:latest` only on
# `latest`.
#
# Version order
# -------------
# `sort -V` alone puts `v0.35.0` before `v0.35.0-alpha`, the opposite of semver. The
# first `-` after X.Y.Z is rewritten to `~` before sorting, and `sort -V` treats `~`
# as lower than the end of the string, so the order is
#   v0.35.0-alpha < v0.35.0-alpha.2 < v0.35.0-rc.1 < v0.35.0
# and the numeric parts still compare as numbers (v0.9.0 < v0.10.0).
# `check_release_tag_ancestry.sh` keeps its plain `sort -V`, which mis-orders a bare
# release against its own pre-releases. No current tag pairs that way (v0.1.0 to
# v0.5.3 have no `-alpha` twin), so that script stays unchanged.

set -euo pipefail

# Same pattern as the `Resolve release inputs` step in pub-release.yml.
SEMVER_PATTERN='^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$'

# stdin: tags, stdout: the same tags with the pre-release separator rewritten.
sort_key() {
    sed -E 's/^(v[0-9]+\.[0-9]+\.[0-9]+)-/\1~/'
}

# Print `latest` or `skip` for $1 against the tags on stdin.
decide() {
    local release_tag=$1 highest key

    if [[ ! "$release_tag" =~ $SEMVER_PATTERN ]]; then
        echo "release_latest_tag: '$release_tag' does not match vX.Y.Z[-suffix]" >&2
        return 2
    fi

    # Tags that are not releases (a stray `vnext`) are ignored rather than ranked.
    highest=$({
        printf '%s\n' "$release_tag"
        cat
    } | grep -E "$SEMVER_PATTERN" | sort_key | LC_ALL=C sort -V | tail -n 1)
    key=$(printf '%s\n' "$release_tag" | sort_key)

    if [ "$highest" = "$key" ]; then
        echo latest
    else
        echo "release_latest_tag: $release_tag is not the highest release tag (${highest//\~/-}); :latest stays unchanged" >&2
        echo skip
    fi
}

self_test() {
    local failures=0

    # expect <expected> <release_tag> [existing tags...]
    expect() {
        local want=$1 release_tag=$2 got
        shift 2
        got=$(printf '%s\n' "$@" | decide "$release_tag" 2>/dev/null)
        if [ "$got" = "$want" ]; then
            echo "ok: $release_tag among [$*] -> $want"
        else
            echo "FAIL: $release_tag among [$*] -> '$got', want '$want'"
            failures=$((failures + 1))
        fi
    }

    expect latest v0.35.0-alpha v0.33.0-alpha v0.34.0-alpha v0.35.0-alpha
    expect skip v0.34.0-alpha v0.33.0-alpha v0.34.0-alpha v0.35.0-alpha
    expect latest v0.1.0-alpha v0.1.0-alpha
    expect latest v0.1.0-alpha
    expect latest v0.36.0-alpha v0.34.0-alpha v0.35.0-alpha
    expect skip v0.35.0-alpha v0.35.0 v0.35.0-alpha v0.35.0-alpha.2 v0.35.0-rc.1
    expect skip v0.35.0-alpha v0.35.0-alpha.2
    expect latest v0.35.0-alpha.2 v0.35.0-alpha
    expect skip v0.35.0-alpha.2 v0.35.0-rc.1
    expect latest v0.35.0-rc.1 v0.35.0-alpha.2
    expect latest v0.35.0 v0.35.0-alpha v0.35.0-alpha.2 v0.35.0-rc.1
    expect skip v0.35.0-rc.1 v0.35.0
    expect latest v0.10.0-alpha v0.9.0-alpha
    expect skip v0.9.0-alpha v0.10.0-alpha
    expect latest v0.35.0-alpha v0.34.0-alpha vnext latest
    expect skip v0.34.0-alpha v0.35.0-alpha vnext

    local out rc
    for bad in '' latest v1 1.2.3 vnext 'v1.2.3 '; do
        rc=0
        out=$(printf 'v0.1.0-alpha\n' | bash "$0" "$bad" 2>/dev/null) || rc=$?
        if [ "$rc" -eq 2 ] && [ -z "$out" ]; then
            echo "ok: bad release tag '$bad' exits 2 with no output"
        else
            echo "FAIL: bad release tag '$bad' gave exit $rc and output '$out'"
            failures=$((failures + 1))
        fi
    done

    if [ "$failures" -gt 0 ]; then
        echo "release_latest_tag self-test: $failures case(s) failed"
        exit 1
    fi
    echo "release_latest_tag self-test: all cases passed"
}

if [ "${1:-}" = "--self-test" ]; then
    self_test
    exit 0
fi

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <release_tag> < existing-tags.txt" >&2
    exit 2
fi

decide "$1"
