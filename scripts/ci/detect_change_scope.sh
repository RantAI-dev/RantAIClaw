#!/usr/bin/env bash
# Detect change scope for CI pipeline.
# Classifies changed files into docs-only, rust, workflow and channel-feature
# categories and writes results to $GITHUB_OUTPUT.
#
# Outputs:
#   docs_only                true when the diff touches documentation only
#   docs_changed             true when the diff touches documentation
#   rust_changed             true when the diff touches Rust
#   workflow_changed         true when the diff touches .github/workflows/
#   channel_features_changed true when the diff can change what the
#                            channel-feature build compiles
#   base_sha                 the commit the diff was taken against
#   docs_files               the changed documentation files
#
# Required environment variables:
#   GITHUB_OUTPUT   — GitHub Actions output file
#   EVENT_NAME      — github.event_name (push or pull_request)
#   BASE_SHA        — base commit SHA to diff against
set -euo pipefail

write_empty_docs_files() {
  {
    echo "docs_files<<EOF"
    echo "EOF"
  } >> "$GITHUB_OUTPUT"
}

# The cargo-feature gates that decide what the channel-feature build
# compiles. A file whose content carries one of them is channel-feature
# scope wherever it lives.
GATE_PATTERN='feature[[:space:]]*=[[:space:]]*"channel-(lark|matrix)"'

# The gate-carrying files at a revision, one repo-root relative path per
# line. `git grep` exits 1 when nothing matches — that is an empty set,
# not a failure — so the exit status is swallowed here.
gate_files_at() {
  local rev="$1"
  git grep --full-name -l -E "$GATE_PATTERN" "$rev" -- 2>/dev/null \
    | sed "s|^${rev}:||" || true
}

# True when the newline-separated list holds exactly this path.
list_contains_path() {
  local list="$1"
  local path="$2"
  local entry
  while IFS= read -r entry; do
    if [ "$entry" = "$path" ]; then
      return 0
    fi
  done <<< "$list"
  return 1
}

BASE="$BASE_SHA"

if [ -z "$BASE" ] || ! git cat-file -e "$BASE^{commit}" 2>/dev/null; then
  {
    echo "docs_only=false"
    echo "docs_changed=false"
    echo "rust_changed=true"
    echo "workflow_changed=false"
    # Fail closed: with no base commit there is no diff to classify, so
    # every scope that can skip a check must stay "changed" and run.
    echo "channel_features_changed=true"
    echo "base_sha="
  } >> "$GITHUB_OUTPUT"
  write_empty_docs_files
  exit 0
fi

CHANGED="$(git diff --name-only "$BASE" HEAD || true)"
if [ -z "$CHANGED" ]; then
  {
    echo "docs_only=false"
    echo "docs_changed=false"
    echo "rust_changed=false"
    echo "workflow_changed=false"
    echo "channel_features_changed=false"
    echo "base_sha=$BASE"
  } >> "$GITHUB_OUTPUT"
  write_empty_docs_files
  exit 0
fi

docs_only=true
docs_changed=false
rust_changed=false
workflow_changed=false
channel_features_changed=false
docs_files=()

# The gate-carrying files at HEAD and at the base commit. Both sides are
# needed: a file that carried a gate at the base and lost it at HEAD
# still changes what the channel-feature build compiles, and a file
# deleted at HEAD is caught by the base-side list.
GATE_FILES_HEAD="$(gate_files_at HEAD)"
GATE_FILES_BASE="$(gate_files_at "$BASE")"

while IFS= read -r file; do
  [ -z "$file" ] && continue

  if [[ "$file" == .github/workflows/* ]]; then
    workflow_changed=true
  fi

  if [[ "$file" == docs/* ]] \
    || [[ "$file" == *.md ]] \
    || [[ "$file" == *.mdx ]] \
    || [[ "$file" == "LICENSE" ]] \
    || [[ "$file" == ".markdownlint-cli2.yaml" ]] \
    || [[ "$file" == .github/ISSUE_TEMPLATE/* ]] \
    || [[ "$file" == .github/pull_request_template.md ]]; then
    if [[ "$file" == *.md ]] \
      || [[ "$file" == *.mdx ]] \
      || [[ "$file" == "LICENSE" ]] \
      || [[ "$file" == .github/pull_request_template.md ]]; then
      docs_changed=true
      docs_files+=("$file")
    fi
    continue
  fi

  docs_only=false

  # `scripts/ci/*` counts as a Rust change: those scripts ARE the Rust gate
  # (quality gate, strict-lint delta, provisioner probe hosts, config readers).
  # Without this, a PR that edits a gate script skips every job that runs it —
  # so the gate lands green having never executed, which is the failure mode
  # these gates exist to prevent.
  #
  # The same rule extends to everything the Rust jobs build, lint or read:
  # `benches/` and `examples/` (only `cargo check --all-targets`, the MSRV
  # job, `clippy --all-targets`, and `cargo bench --no-run` compile them);
  # the cargo and lint configs that change the build/linker (`Cargo.toml`,
  # `Cargo.lock`, `.cargo/config.toml`, `clippy.toml`, `rustfmt.toml`,
  # `deny.toml`); and `build.rs`, which cargo invokes itself. A `.md` under
  # any of these paths still matches the docs branch above and stays
  # docs-only — the docs check runs first, so it never reaches this clause.
  #
  # `.github/workflows/ci-run.yml` is also a Rust change: this workflow file
  # is what selects which Rust jobs run and what they run. Skipping those
  # jobs on a workflow edit lets the workflow itself land green without ever
  # executing — the same failure mode the `scripts/ci/*` clause prevents.
  # Other workflow files (pr-labeler.yml, sec-audit.yml, test-fuzz.yml, …)
  # do not change what the Rust jobs build and stay workflow-only — routing
  # them into the full Rust pipeline wastes the Rust jobs' run time on a
  # workflow-only edit.
  if [[ "$file" == src/* ]] \
    || [[ "$file" == tests/* ]] \
    || [[ "$file" == benches/* ]] \
    || [[ "$file" == examples/* ]] \
    || [[ "$file" == scripts/ci/* ]] \
    || [[ "$file" == ".github/workflows/ci-run.yml" ]] \
    || [[ "$file" == "Cargo.toml" ]] \
    || [[ "$file" == "Cargo.lock" ]] \
    || [[ "$file" == "deny.toml" ]] \
    || [[ "$file" == ".cargo/config.toml" ]] \
    || [[ "$file" == "clippy.toml" ]] \
    || [[ "$file" == "rustfmt.toml" ]] \
    || [[ "$file" == "build.rs" ]]; then
    rust_changed=true
  fi

  # `channel_features_changed` is the narrower flag: the files that can
  # change what the channel-feature build compiles, or which filter the
  # channel-features job runs. A pull request that touches none of them
  # cannot change the outcome of that build, so the job's test steps are
  # skipped by design.
  #
  # The set is not just "the Lark and Matrix modules". `channel-matrix` is
  # not a default feature, so a body under `#[cfg(feature = "channel-matrix")]`
  # is compiled by no other job — not lint, not test, not the feature
  # matrix. A file carrying such a gate belongs in this set, or the Matrix
  # build goes unchecked on that pull request.
  #
  # The explicit paths below cover the modules, the manifest and lockfile
  # that decide the feature set, the workflow that selects the job and the
  # CI scripts the job calls. The content rule that follows them is what
  # keeps the set from going stale when a new file starts carrying a gate;
  # it consults HEAD and the base commit, so removing a gate — or deleting
  # the file that held it — counts as a change to the build.
  #
  # This branch sits below the docs branch above, so a documentation edit
  # under one of these paths stays docs-only and does not pull the channel
  # build in.
  if [[ "$file" == "src/channels/lark.rs" ]] \
    || [[ "$file" == src/channels/lark/* ]] \
    || [[ "$file" == "src/channels/matrix.rs" ]] \
    || [[ "$file" == src/channels/matrix/* ]] \
    || [[ "$file" == "src/channels/mod.rs" ]] \
    || [[ "$file" == "Cargo.toml" ]] \
    || [[ "$file" == "Cargo.lock" ]] \
    || [[ "$file" == ".github/workflows/ci-run.yml" ]] \
    || [[ "$file" == scripts/ci/* ]]; then
    channel_features_changed=true
  fi

  if list_contains_path "$GATE_FILES_HEAD" "$file" \
    || list_contains_path "$GATE_FILES_BASE" "$file"; then
    channel_features_changed=true
  fi
done <<< "$CHANGED"

{
  echo "docs_only=$docs_only"
  echo "docs_changed=$docs_changed"
  echo "rust_changed=$rust_changed"
  echo "workflow_changed=$workflow_changed"
  echo "channel_features_changed=$channel_features_changed"
  echo "base_sha=$BASE"
  echo "docs_files<<EOF"
  printf '%s\n' "${docs_files[@]}"
  echo "EOF"
} >> "$GITHUB_OUTPUT"
