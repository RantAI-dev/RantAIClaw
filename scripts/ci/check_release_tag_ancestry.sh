#!/usr/bin/env bash
# Assert every release tag still points at a commit reachable from `main`.
#
# Usage: scripts/ci/check_release_tag_ancestry.sh [--self-test]
#
# Why this exists
# ---------------
# `pub-release.yml` already refuses to publish a tag that is not an ancestor of
# `origin/main`. That guard runs at release time and it works. What it cannot
# see is a rewrite performed *after* a release: on 2026-09-07 `origin/main` was
# force-pushed to strip commit trailers, and `v0.30.0-alpha` — already published,
# with a **signed** `release-manifest.json` naming its commit — stopped being an
# ancestor of `main`. Nothing noticed. It was found by reading.
#
# This check runs on pull requests, so the next one is found in hours.
#
# About the exemption list
# ------------------------
# It has exactly two entries and it is closed. Both are content-identical to
# `main` and both are named by signed release assets, so moving either tag would
# put it in conflict with its own provenance — that is why they are exempt
# rather than fixed. See `docs/contributing/release-process.md`, "Known-divergent
# releases". A third entry is not a maintenance step; it means published history
# was rewritten again, which the same document says not to do.

set -euo pipefail

# tag<TAB>why. Adding a line here requires the reason to survive review.
KNOWN_DIVERGENT=$(
    cat <<'EOF'
v0.30.0-alpha	2026-09-07 trailer-strip rewrite; trees identical to main; release-manifest.json is a signed asset naming 94d7b18
EOF
)

BASE_REF="${BASE_REF:-origin/main}"

is_known() {
    printf '%s\n' "$KNOWN_DIVERGENT" | grep -q "^$1	"
}

run_check() {
    local failures=0 checked=0

    if ! git rev-parse --verify --quiet "$BASE_REF" >/dev/null; then
        echo "ERROR: $BASE_REF is not available. Fetch it first (checkout with fetch-depth: 0)." >&2
        return 2
    fi

    local tag
    while IFS= read -r tag; do
        [ -n "$tag" ] || continue
        checked=$((checked + 1))
        if git merge-base --is-ancestor "$tag" "$BASE_REF" 2>/dev/null; then
            continue
        fi
        if is_known "$tag"; then
            echo "known:   $tag is not an ancestor of $BASE_REF — recorded in release-process.md"
            continue
        fi
        echo "ERROR:   release tag $tag is not an ancestor of $BASE_REF."
        echo "         Its commit is not on main, so a bisect across that release has no path"
        echo "         and \`git describe\` cannot name it. Published history was probably"
        echo "         rewritten — see docs/contributing/release-process.md."
        failures=$((failures + 1))
    done < <(git tag --list 'v*' | sort -V)

    echo "checked $checked release tag(s); $failures unrecorded divergence(s)"
    [ "$failures" -eq 0 ]
}

# --self-test proves the two arms without needing a broken repository: a tag that
# IS an ancestor must pass, and the exemption must be the only thing keeping the
# known-divergent one green. A guard nobody has seen reject anything is not a guard.
if [ "${1:-}" = "--self-test" ]; then
    failures=0

    if is_known "v0.30.0-alpha"; then
        echo "ok: the recorded divergence is recognised"
    else
        echo "FAIL: v0.30.0-alpha should be in KNOWN_DIVERGENT"
        failures=$((failures + 1))
    fi

    if is_known "v0.29.0-alpha"; then
        echo "FAIL: v0.29.0-alpha must NOT be exempt — it is a proper ancestor"
        failures=$((failures + 1))
    else
        echo "ok: a healthy tag is not exempt"
    fi

    if is_known "v0.30.0-alpha-suffix"; then
        echo "FAIL: exemption matching must be exact, not a prefix"
        failures=$((failures + 1))
    else
        echo "ok: exemption matching is exact"
    fi

    if [ "$failures" -gt 0 ]; then
        echo "check_release_tag_ancestry self-test: $failures case(s) failed"
        exit 1
    fi
    echo "check_release_tag_ancestry self-test: all cases passed"
    exit 0
fi

run_check
