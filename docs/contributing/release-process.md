# RantaiClaw Release Process

This runbook defines the maintainers' standard release flow.

Last verified: **August 8, 2026**, against `v0.18.1-alpha`.

## Release Goals

- Keep releases predictable and repeatable.
- Publish only from code already in `main`.
- Verify multi-target artifacts before publish.
- Keep release cadence regular even with high PR volume.

## Standard Cadence

- Patch/minor releases: weekly or bi-weekly.
- Emergency security fixes: out-of-band.
- Never wait for very large commit batches to accumulate.

## Workflow Contract

Release automation lives in:

- `.github/workflows/pub-release.yml`

Modes:

- Tag push `v*`: publish mode.
- Manual dispatch: verification-only or publish mode.
- Weekly schedule: verification-only mode.

Publish-mode guardrails:

- Tag must match semver-like format `vX.Y.Z[-suffix]`.
- Tag must already exist on origin.
- Tag commit must be reachable from `origin/main`.
- Artifacts are verified before publish.

The GHCR image is **not** a guardrail, and does not gate the release. `Pub
Release` pushes `ghcr.io/<owner>/<repo>:<tag>` from the same job, but *after*
`Create GitHub Release`, and the push step carries `continue-on-error: true`.
That ordering is deliberate: before v0.6.50 the multi-arch image build ran under
QEMU and regularly exceeded the 90-minute job timeout, which cancelled the runner
and left already-built, already-signed binaries unpublished. The release page now
goes live the moment cosign finishes, and the image can take its time — or fail —
without taking the release with it.

The practical consequence is in step 6: a green `Publish Release` job does **not**
mean the image was pushed.

## Maintainer Procedure

### 1) Preflight on `main`

1. Ensure required checks are green on latest `main`.
2. Confirm no high-priority incidents or known regressions are open.
3. Confirm installer and Docker workflows are healthy on recent `main` commits.

### 2) Land the version bump on `main`

The tag is cut from a commit, so the version has to be in `main` before you tag —
`cut_release_tag.sh` will otherwise stamp a release whose `Cargo.toml` still
claims the previous version. Open an ordinary PR touching exactly three files:

- `Cargo.toml` — the new version
- `Cargo.lock` — regenerate with `cargo check --offline` (`--locked` refuses, by design)
- `CHANGELOG.md` — a new section for the version

Choose the bump by what the release actually contains, not by how much work went
into it: a new CLI surface or a changed API contract is minor; anything else is
patch. Say plainly at the top of the changelog entry when a release changes
nothing an operator can observe.

Run the release gates locally before opening it — these are what
`verify-update-cycle` runs, and they are cheap:

```bash
cargo test --locked --test schema_drift --test config_migration_roundtrip
cargo test --locked --lib config::migrations
cargo test --locked --lib sessions::migrations
```

`schema_drift` passing **without** a snapshot update is the machine-checkable
statement that the config schema did not move, and therefore that the release
carries no migration and rolls back cleanly.

### 3) Run verification build (no publish)

Run `Pub Release` manually:

- `publish_release`: `false`
- `release_ref`: `main`

Expected outcome:

- Full target matrix builds successfully.
- `verify-artifacts` confirms all expected archives exist.
- No GitHub Release is published.

### 4) Cut release tag

From a clean local checkout synced to `origin/main`:

```bash
scripts/release/cut_release_tag.sh vX.Y.Z --push
```

This script enforces:

- clean working tree
- `HEAD == origin/main`
- non-duplicate tag
- semver-like tag format

### 5) Monitor publish run

A tag push starts exactly two workflows: `Pub Release` and `Workflow Sanity`.
`Pub Docker Img` is **not** one of them — its triggers are `push` to `main`,
pull requests, and manual dispatch, so it never sees a tag ref. The release image
comes from `Pub Release` itself; nothing is missing when `Pub Docker Img` stays
quiet during a release.

Monitor `Pub Release` in publish mode.

Expected publish outputs:

- release archives
- `SHA256SUMS`
- `CycloneDX` and `SPDX` SBOMs
- cosign signatures/certificates
- GitHub Release notes + assets

### 6) Post-release validation

1. Download one release archive and check it against `SHA256SUMS`, then run the
   extracted binary and confirm `--version` reports the tag you cut. A green
   workflow says the artifact was built, not that it is intact or correct.
2. Confirm the release image was pushed. Because the push step is
   `continue-on-error`, the job conclusion cannot tell you — read the **step**:

   ```bash
   run=$(gh run list --workflow="Pub Release" --limit 1 --json databaseId -q '.[0].databaseId')
   job=$(gh run view "$run" --json jobs -q '.jobs[] | select(.name=="Publish Release") | .databaseId')
   gh api "repos/<owner>/<repo>/actions/jobs/$job" \
     -q '.steps[] | select(.name=="Build and push release Docker image") | .conclusion'
   ```

   Anything other than `success` means the tag exists but the image does not.
   Re-run that step or push the image manually; the release itself is unaffected.

   Reading GHCR directly needs a token with the `read:packages` scope. Without
   it both `ghcr.io/token` and the registry API answer 401/403 for a missing
   image *and* for one you simply cannot see, so a failed lookup proves nothing.
3. Verify install paths that rely on release assets (for example bootstrap binary download).

## Emergency / Recovery Path

If tag-push release fails after artifacts are validated:

1. Fix workflow or packaging issue on `main`.
2. Re-run manual `Pub Release` in publish mode with:
   - `publish_release=true`
   - `release_tag=<existing tag>`
   - `release_ref` is automatically pinned to `release_tag` in publish mode
3. Re-validate released assets.

## Operational Notes

- Keep release changes small and reversible.
- Prefer one release issue/checklist per version so handoff is clear.
- Avoid publishing from ad-hoc feature branches.
