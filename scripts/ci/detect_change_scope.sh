#!/usr/bin/env bash
# Detect change scope for CI pipeline.
# Classifies changed files into docs-only, rust, workflow categories
# and writes results to $GITHUB_OUTPUT.
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

BASE="$BASE_SHA"

if [ -z "$BASE" ] || ! git cat-file -e "$BASE^{commit}" 2>/dev/null; then
  {
    echo "docs_only=false"
    echo "docs_changed=false"
    echo "rust_changed=true"
    echo "workflow_changed=false"
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
    echo "base_sha=$BASE"
  } >> "$GITHUB_OUTPUT"
  write_empty_docs_files
  exit 0
fi

docs_only=true
docs_changed=false
rust_changed=false
workflow_changed=false
docs_files=()
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
done <<< "$CHANGED"

{
  echo "docs_only=$docs_only"
  echo "docs_changed=$docs_changed"
  echo "rust_changed=$rust_changed"
  echo "workflow_changed=$workflow_changed"
  echo "base_sha=$BASE"
  echo "docs_files<<EOF"
  printf '%s\n' "${docs_files[@]}"
  echo "EOF"
} >> "$GITHUB_OUTPUT"
