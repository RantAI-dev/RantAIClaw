#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/release/cut_release_tag.sh <tag> [--push]

Create an annotated release tag from the current checkout.

Requirements:
- tag must match vX.Y.Z (optional suffix like -rc.1)
- working tree must be clean
- tag must match the [package] version in Cargo.toml
- HEAD must match origin/main
- tag must not already exist locally or on origin

Options:
  --push   Push the tag to origin after creating it
USAGE
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
  usage
  exit 1
fi

TAG="$1"
PUSH_TAG="false"
if [[ $# -eq 2 ]]; then
  if [[ "$2" != "--push" ]]; then
    usage
    exit 1
  fi
  PUSH_TAG="true"
fi

SEMVER_PATTERN='^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$'
if [[ ! "$TAG" =~ $SEMVER_PATTERN ]]; then
  echo "error: tag must match vX.Y.Z or vX.Y.Z-suffix (received: $TAG)" >&2
  exit 1
fi

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "error: run this script inside the git repository" >&2
  exit 1
fi

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "error: working tree is not clean; commit or stash changes first" >&2
  exit 1
fi

# The tag names a version, and until now nothing checked that it is the version
# the binaries will actually report. v0.5.1 was cut from a commit whose
# Cargo.toml still read 0.5.0; that release is published, and its binary answers
# `rantaiclaw 0.5.0` to this day. Nothing caught it at tag time, nothing caught
# it at publish time, and the only thing that would have is downloading the
# artefact after the tag was signed.
REPO_ROOT="$(git rev-parse --show-toplevel)"
CARGO_VERSION="$(
  awk '/^\[package\]/ { in_package = 1; next }
       /^\[/           { in_package = 0 }
       in_package && /^version[[:space:]]*=/ {
           sub(/^version[[:space:]]*=[[:space:]]*"/, "")
           sub(/".*$/, "")
           print
           exit
       }' "$REPO_ROOT/Cargo.toml"
)"

if [[ -z "$CARGO_VERSION" ]]; then
  echo "error: could not read the [package] version from $REPO_ROOT/Cargo.toml" >&2
  exit 1
fi

if [[ "$TAG" != "v$CARGO_VERSION" ]]; then
  echo "error: tag does not match the crate version." >&2
  echo "  tag:                $TAG" >&2
  echo "  Cargo.toml version: $CARGO_VERSION  (expected tag: v$CARGO_VERSION)" >&2
  echo "hint: land the version bump on main first — docs/contributing/release-process.md, \"Land the version bump on main\"." >&2
  exit 1
fi

echo "Fetching origin/main and tags..."
git fetch --quiet origin main --tags

HEAD_SHA="$(git rev-parse HEAD)"
MAIN_SHA="$(git rev-parse origin/main)"
if [[ "$HEAD_SHA" != "$MAIN_SHA" ]]; then
  echo "error: HEAD ($HEAD_SHA) is not origin/main ($MAIN_SHA)." >&2
  echo "hint: checkout/update main before cutting a release tag." >&2
  exit 1
fi

if git show-ref --tags --verify --quiet "refs/tags/$TAG"; then
  echo "error: tag already exists locally: $TAG" >&2
  exit 1
fi

if git ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null 2>&1; then
  echo "error: tag already exists on origin: $TAG" >&2
  exit 1
fi

MESSAGE="rantaiclaw $TAG"
git tag -a "$TAG" -m "$MESSAGE"
echo "Created annotated tag: $TAG"

if [[ "$PUSH_TAG" == "true" ]]; then
  git push origin "$TAG"
  echo "Pushed tag to origin: $TAG"
  echo "GitHub release pipeline will run via .github/workflows/pub-release.yml"
else
  echo "Next step: git push origin $TAG"
fi
