# RantaiClaw Release Process

This runbook defines the maintainers' standard release flow.

**Maintainer Procedure last verified: September 4, 2026**, against `v0.28.0-alpha` — every
step in that section was run, including the verification build, the tag script, and the
post-release checks. That date covers the procedure only.

The maturity contract and the rewrite rule below were **written 2026-09-07** and are policy,
not a procedure that has been executed. Where they cite repository state — tag ancestry,
merge-bases, tree identity — those were checked on that date with the commands shown.

## Release Goals

- Keep releases predictable and repeatable.
- Publish only from code already in `main`.
- Verify multi-target artifacts before publish.
- Keep release cadence regular even with high PR volume.

## What alpha means, and what would end it

Over a hundred `-alpha` releases have shipped and this document said nothing about what the
label warned anyone about. A label that never changes stops carrying information, so here is
what it currently means and what would have to be true to drop it.

Note that **the update channel's `stable` is a different word.** `rantaiclaw update
--channel stable` filters on GitHub's *prerelease* flag, not on the version suffix
(`src/lifecycle/update.rs:546`). Every release so far is tagged `-alpha` and published with
that flag off, so `stable` — the default channel — installs alpha builds. That is a
packaging detail; the maturity below is the product claim.

### alpha — where the project is now

**What an operator is being warned about**, each of which happened in the last three releases:

- **The config schema may move under you.** v28 → v31 across three releases. Migrations are
  written and tested, but a downgrade after an upgrade is not supported.
- **Rollback may not be clean.** `rantaiclaw rollback` restores the previous binary and a
  config snapshot; it does not migrate a newer on-disk schema back down.
- **Defaults may change.** The `[cost] max_tokens_per_day` ceiling switched *itself on* at
  2,000,000 tokens/day for installs that had never set it.
- **Not every channel is verified.** Four are supported (Telegram, Discord, Slack, WhatsApp
  Cloud); the rest ship labelled *under development*.

**Suitable for**: evaluation, personal use, non-critical internal automation where an
operator reads the changelog before upgrading.

**Not suitable for**: anything where an unattended upgrade is expected to be safe.

### beta — the exit criteria from alpha

All of these, measured, not asserted:

| Criterion | How it is checked |
|---|---|
| **No schema break within a minor version.** A `0.x.y` → `0.x.z` upgrade never migrates. | The schema-drift gate already runs per release; add the minor-version comparison to it. |
| **Three consecutive releases with no P0 hotfix.** | Release history. A P0 is anything that made a released binary unusable for a default install. |
| **Every supported-tier channel verified live** against a real workspace, per release. | A recorded run per channel, not a passing unit test. |
| **Memory stable over a 72-hour soak** at a defined message rate, with RSS recorded at start and end. | A soak run whose numbers go in the release notes. |
| **Clean rollback across one version boundary**, verified by actually doing it. | Upgrade, use, roll back, confirm the profile still loads. |
| **Documented footprint** — binary size per target, idle RSS, cold start — measured for the release being cut. | `docs/pillars/8-install-release.md`, with the date and method. |

### stable — the exit criteria from beta

| Criterion | How it is checked |
|---|---|
| **Six months in beta** with no criterion above regressing. | Release history. |
| **A deprecation policy exists and has been honoured once** — a config key removed with a release of warning first. | The key's removal commit and the release that warned. |
| **Security reports have a measured response time** against `SECURITY.md`'s 7-day acknowledgement aim. | The advisory record. |
| **The supported channel tier is unchanged for three releases.** | `CHANNEL_CATALOG`. |
| **`--all-features` builds in CI.** | A CI job, not a local run. |

Waves 4 and 5 of the production-readiness effort are judged against this table. It is written
before them on purpose: exit criteria invented after the work they measure are not criteria.

## Published history is not rewritten

On 2026-09-07 `origin/main` was force-pushed in **both** repositories to strip
`Co-authored-by: Claude` / `Claude-Session:` trailers from 22 commits. The trees were
byte-identical and the shipped bytes were correct, but the release tags had been cut *before*
the rewrite, so they still point at pre-rewrite commits that are no longer on `main`.

**The rule, going forward: do not rewrite published history.** If it is ever unavoidable,
decide *before* the push which of these applies to every tag pointing into the rewritten
range, and record the choice:

1. re-cut the tag onto the rewritten commit — only if nothing signed names the old SHA; or
2. preserve the pre-rewrite commit by **pushing a branch** that keeps it reachable.

The release workflow already refuses a tag that is not reachable from `origin/main`
(`.github/workflows/pub-release.yml`, in the `prepare` job). That guard runs at release time
and it works — verified 2026-09-07 against the current repository state, where it rejects
`v0.30.0-alpha` and passes `v0.29.0-alpha`. What it cannot catch is a rewrite performed
*after* a release, which is exactly what happened. Hence the rule above, and the check in
`scripts/ci/check_release_tag_ancestry.sh`.

### Known-divergent releases (content-identical, not to be re-cut)

| Repo | Tag | Tagged commit | Merge-base with `main` | Trees |
|---|---|---|---|---|
| RantaiClaw | `v0.30.0-alpha` | `94d7b18` | `4df441f` (`v0.29.0-alpha`) | identical (`git diff 94d7b18 ae5924a` is empty) |
| claw-ui | `v0.3.27` | `0f12c71` | `eaaa002` (`v0.3.26`) | identical |

**Neither tag is to be moved or deleted.** RantaiClaw's `release-manifest.json` is itself a
**signed** release asset naming `"source_sha": "94d7b18…"`, and claw-ui `v0.3.27` ships a
cosign `.bundle`. Moving either tag would put it in conflict with its own signed provenance.

Both pre-rewrite commits are now kept reachable independently of their tags by
`backup/pre-trailer-strip-2026-09-07`, **pushed to `origin` in both repositories on
2026-09-07**. Before that push they survived only because a tag pointed at them, which meant
one accidental tag deletion would have left a signed artefact naming a commit that no longer
existed. Do not delete that branch either.

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

These gates say the release-bump PR itself is consistent. They do **not** tell
you whether the release rolls back cleanly, and reading them that way is a
mistake this runbook used to make: `schema_drift` compares the working tree
against the snapshots committed in `tests/snapshots/`, so it measures drift
**since the last PR that moved the schema** — not since the last release. The
PR that bumped the schema also committed the matching snapshot, so by the time
you cut the tag the gate is green again and stays green.

To find out what the release actually carries, compare the schema version at
the previous tag with the one you are about to ship:

```bash
prev=$(git describe --tags --abbrev=0)
git show "$prev":src/config/migrations.rs | grep 'pub const CURRENT_VERSION'
grep 'pub const CURRENT_VERSION' src/config/migrations.rs
```

Different numbers mean the release carries a migration and does **not** roll
back cleanly: a config written by the new binary will not load on the old one.
Say so at the top of the changelog entry, naming both versions — 0.26.0, 0.27.0
and 0.28.0 all do. Same check against `src/sessions/migrations.rs` for the
sessions store.

v0.28.0-alpha is the worked example: all three gates were green, and the
release still moved the config schema from v26 to v27.

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
