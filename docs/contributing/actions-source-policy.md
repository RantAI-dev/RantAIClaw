# Actions Source Policy (Phase 2)

This document defines the GitHub Actions source-control policy for this repository.

Phase 1 restricted *which* actions could run, by owner allowlist. **Phase 2 fixes *what*
runs, by digest.** Every `uses:` in `.github/workflows/` is pinned to a 40-character commit
SHA, and no workflow pipes a remote script into a shell.

## Current Policy

- Repository Actions permissions: enabled
- Allowed actions mode: selected
- **SHA pinning required: yes — enforced in CI**

Pinning is not a convention here; it is a check. `Workflow Sanity (pinned sources)` in
`.github/workflows/workflow-sanity.yml` fails a pull request that introduces either

- a `uses:` reference that is not a 40-character commit SHA (local `./` composite actions
  and `docker://` references are exempt — neither has a tag that can move), or
- a `curl`/`wget` whose output is piped into `sh`/`bash`.

Reproduce it locally with the same logic the job runs:

```bash
grep -rn 'uses:' .github/workflows/ | grep -vE '@[0-9a-f]{40}'   # expect no output
grep -rnE '\b(curl|wget)\b[^|]*\|\s*(sudo\s+)?(ba|z|d)?sh\b' .github/workflows/
```

### Why the release job sets the rule

`pub-release.yml`'s `publish` job holds `contents: write`, `packages: write`,
`id-token: write` and `attestations: write`, and it mints the cosign signatures users are
told to trust. Anything it executes runs beside those credentials. A moving tag and a piped
installer script are the same hole in different clothing: in both, the code that actually
runs is chosen *after* review. So the rule that matters is one sentence — **anything
executed inside a job that holds signing credentials is pinned by digest.**

Non-action binaries that job installs are pinned the same way, by version and SHA-256, not
by an installer script:

- `syft` — version and checksum are literals in the step's `env:`; bump both together.

## Actions in use

Derived from the workflows, not maintained by hand:

```bash
grep -rhoP 'uses:\s*\K[\w.-]+/[\w.-]+' .github/workflows/ | sort -u
```

As of 2026-09-07 that is: `actions/checkout`, `actions/download-artifact`,
`actions/github-script`, `actions/labeler`, `actions/upload-artifact`,
`docker/build-push-action`, `docker/login-action`, `docker/metadata-action`,
`docker/setup-buildx-action`, `dtolnay/rust-toolchain`, `EmbarkStudios/cargo-deny-action`,
`github/codeql-action`, `lycheeverse/lychee-action`, `rhysd/actionlint`,
`rustsec/audit-check`, `sigstore/cosign-installer`, `softprops/action-gh-release`,
`Swatinem/rust-cache`.

Each pinned `uses:` carries a trailing `# vX.Y.Z` comment naming the release the SHA belongs
to. The comment is documentation; the SHA is the contract. When they disagree, the SHA wins —
and the comment is a bug, because a reviewer auditing a pin reads the comment.

> **Unverified from this session (2026-09-07):** the repository-side allowlist could not be
> re-exported — `gh api repos/RantAI-dev/RantAIClaw/actions/permissions` returns HTTP 403
> without admin or the Actions-policy fine-grained permission. The allowlist recorded in
> Phase 1 had drifted from the workflows in both directions: it listed `useblacksmith/*` and
> `DavidAnson/markdownlint-cli2-action@*`, which no workflow uses, and omitted `Swatinem/*`
> and `github/codeql-action`, which they do. Someone with admin should export the effective
> policy and reconcile it against the list above.

## Change Control Export

Use these commands to export the current effective policy for audit/change control:

```bash
gh api repos/RantAI-dev/RantAIClaw/actions/permissions
gh api repos/RantAI-dev/RantAIClaw/actions/permissions/selected-actions
```

Record each policy change with:

- change date/time (UTC)
- actor
- reason
- allowlist delta (added/removed patterns)
- rollback note

## Agentic Workflow Guardrails

Because this repository has high agent-authored change volume:

- Any PR that adds or changes `uses:` action sources must include an allowlist impact note.
- New third-party actions require explicit maintainer review before allowlisting.
- Pin the SHA and name the release in the comment; do not pin to a tag "temporarily".
- Expand allowlist only for verified missing actions; avoid broad wildcard exceptions.
- Keep rollback instructions in the PR description for Actions policy changes.

### Bumping a pinned action

```bash
# resolve the commit a tag points at, then paste the SHA and the tag into the comment
curl -sSfL https://api.github.com/repos/<owner>/<repo>/commits/<tag> | jq -r .sha
```

Stay within the current major unless the bump is the point of the pull request. A pin change
and a version upgrade are two different reviews.

## Validation Checklist

After allowlist or pin changes, validate:

1. `CI`
2. `Docker`
3. `Security Audit`
4. `Workflow Sanity` (includes the pinned-sources job)
5. `Release` (when safe to run)

Failure mode to watch for:

- `action is not allowed by policy`

If encountered, add only the specific trusted missing action, rerun, and document why.

## Sweep notes

- 2026-09-07: **Phase 2 landed.** Eighteen tag-pinned `uses:` (ten `Swatinem/rust-cache@v2`,
  four `docker/setup-buildx-action@v3`, three `docker/build-push-action@v6`, one
  `docker/login-action@v3`) pinned to commit SHAs within their existing major — a pin, not an
  upgrade. The `syft` installer in the signing job, previously
  `curl … anchore/syft/main/install.sh | sh`, replaced with a version-pinned,
  SHA-256-verified release tarball. Two pins in `pub-docker-img.yml` carried version comments
  that named `v4` while the SHA was `actions/upload-artifact` v6.0.0 and
  `actions/download-artifact` v7.0.0; comments corrected. `Workflow Sanity (pinned sources)`
  added so none of this can silently regress.
- 2026-02-17: Rust dependency cache recorded as migrated from `Swatinem/rust-cache` to
  `useblacksmith/rust-cache`. **This was later reverted without a note** — no workflow
  references `useblacksmith` as of 2026-09-07, and `Swatinem/rust-cache` is used ten times.
- 2026-02-16: Hidden dependency discovered in `release.yml`: `sigstore/cosign-installer@...`
    - Added allowlist pattern: `sigstore/cosign-installer@*`
- 2026-02-16: Blacksmith migration blocked workflow execution
    - Added allowlist pattern: `useblacksmith/*` for self-hosted runner infrastructure
    - Actions: `useblacksmith/setup-docker-builder@v1`, `useblacksmith/build-push-action@v2`
- 2026-02-17: Security audit reproducibility/freshness balance update
    - Added allowlist pattern: `rustsec/audit-check@*`
    - Replaced inline `cargo install cargo-audit` execution with pinned `rustsec/audit-check@69366f33c96575abad1ee0dba8212993eecbe998` in `security.yml`
    - Supersedes floating-version proposal in #588 while keeping action source policy explicit

## Rollback

Emergency unblock path:

1. Temporarily set Actions policy back to `all`.
2. Restore selected allowlist after identifying missing entries.
3. Record incident and final allowlist delta.

Reverting a pin is a one-line change per `uses:`; the pinned-sources job will reject it, which
is the intended friction.
