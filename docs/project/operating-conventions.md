# Operating conventions

How rantaiclaw alpha cuts ship. Extracted from the v0.6.30 handoff
plan (since archived) so the rules don't get lost when individual
plan docs roll over. Don't deviate without a good reason; if you
need to, write the reason in the commit body.

## Build target

- **Always** build `--target x86_64-unknown-linux-musl --release`.
  Static musl is the canonical artifact; non-musl builds aren't
  shipped. The pub-release matrix cross-builds for darwin / windows
  / aarch64 / armv7 on top of that.

## Validation per change

- `cargo check` — must be clean (no errors) before anything else.
- `cargo test --lib <module-path>` — narrow run on what you touched;
  **do not** run the full suite locally (slow + memory-pressured
  test machine). CI runs the full set on push.
- TUI changes — verify in tmux. `dev/tui-smoke.sh` exists if you want
  the scripted path; otherwise launch the release binary directly
  and exercise the affected slash commands.
- Agent-loop changes — live-test against the configured provider
  (MiniMax by default; key in `.env`, load with
  `set -a; source .env; set +a`).

## Ship sequence per drop

1. Bump `Cargo.toml` `version = "0.6.N-alpha"` (or the next
   appropriate semver).
2. `cargo fmt --all && cargo build --release --target x86_64-unknown-linux-musl`.
3. Commit with the structured message convention below; push to
   `main` (solo-maintainer policy — direct push is fine).
4. Tag `vX.Y.Z` at the commit; `git push origin vX.Y.Z`.
5. `gh release create vX.Y.Z --target main --prerelease --notes …`.
   The `pub-release.yml` workflow builds + attaches binaries for all
   six target platforms.

## Commit message convention

One logical drop per commit:

```
feat(<scope>): <one-line summary> — bump v0.6.N-alpha

<2–4 line context: what was the gap, why now>

## What landed
- <bullets>

## Verification
- <how it was tested>

## Trade-offs / deferred
- <any compromises>
```

`scope` follows conventional-commit; for non-feature work use
`fix(...)`, `refactor(...)`, `chore(...)`, `docs(...)`, `style(...)`,
`ci(...)`, `test(...)`. The PR-title-lint workflow enforces the
prefix.

## Adding a schema migration

`config.toml` and `sessions.db` are versioned. A field rename or
type change across releases would otherwise leave users on the
previous binary with an unparseable config or a broken DB. The
migration framework keeps that from ever happening.

### Config (`config.toml`)

Source: `src/config/migrations.rs`. Runs on every `Config::load_or_init`
between the raw-TOML read and the `Config` deserialise.

When a future cut needs to rename or re-type a field:

1. Bump `CURRENT_VERSION` in `src/config/migrations.rs` from `N` to
   `N+1`.
2. Add `migrate_vN_plus_1(raw: &mut toml::Value) -> Result<()>` next
   to the existing migration stubs. Keep the transformation narrow —
   one schema change per `migrate_vN`.
3. Append `if from < N+1 { migrate_vN_plus_1(raw)?; }` to the chain
   inside `migrate()`.
4. Add a unit test that feeds a fixture written by an old binary
   through `migrate()` and asserts the post-shape.

### Sessions DB (SQLite)

Source: `src/sessions/migrations.rs`. Runs on every
`SessionStore::open` after the connection is opened.

When a future cut needs a new index, table, or DDL change:

1. Bump `CURRENT_VERSION` from `N` to `N+1`.
2. Add `migrate_vN_plus_1(conn: &Connection) -> Result<()>` that
   issues the DDL via `execute_batch`. End by calling
   `set_schema_version(conn, N+1)`.
3. Append `if version < N+1 { migrate_vN_plus_1(conn)?; }` to the
   chain inside `run_migrations()`.
4. Add a unit test that opens an in-memory connection through the
   prior version, runs the new migration, and asserts the new
   schema is present.

### Both frameworks

- Each migration is **idempotent**: running it twice has no extra
  effect. Tests assert this.
- Each migration is **append-only**: never re-edit a shipped
  `migrate_vN` — the next user's `config.toml` already encodes that
  it ran. If a migration is wrong, write a `migrate_vN_plus_1` that
  corrects it.
- A binary refuses to load state newer than its own
  `CURRENT_VERSION` (no silent downgrade). The user has to update
  their binary first.

### Enforcement — the tests that fail loud

Two integration tests sit between schema changes and the release
button:

1. **`tests/schema_drift.rs`** — snapshot tests over the
   `Config` JSON Schema and the sessions.db DDL. Any field rename,
   type change, new field, removed field, or DDL drift fails these
   tests with a "you changed the schema" message. The maintainer's
   options are:
   - Run `cargo insta accept` to refresh the snapshot. Forces the
     maintainer to *consciously* acknowledge the schema moved — a
     reviewer can spot the snapshot update in the PR diff and ask
     "where's the migration?"
   - Bump `CURRENT_VERSION` in the relevant `migrations.rs` module.
   - Add the `migrate_vN` + chain arm + unit test.
2. **`tests/config_migration_roundtrip.rs`** — serialises
   `Config::default()`, strips `schema_version`, runs `migrate()`,
   and asserts the post-migration TOML still deserialises into
   `Config`. If a new required field lacks both `#[serde(default)]`
   and a `migrate_vN` that fills it in, this test fails with a
   message naming the offending field. The fix is one of:
   - Add `#[serde(default)]` to the new field, or
   - Add a `migrate_vN` that injects a default value, then bump
     `CURRENT_VERSION`.

Both tests run on every `cargo test`. CI is the backstop: a PR can't
land if it broke either of them. The snapshot file lives in
`tests/snapshots/` and shows up in diffs whenever the schema moves.

### Release-side reinforcement

The same maintainer discipline matters at release time. Two gates
sit inside `pub-release.yml` before any binary gets built:

1. **`verify-update-cycle` job** (v0.6.47+) — runs the two schema-
   drift tests, the migration round-trip, and the per-store
   migration unit tests. The platform build matrix has
   `needs: [prepare, verify-update-cycle]`, so a red here means no
   binary is ever cross-compiled and no release is ever published.
   The workflow is triggered by the tag itself — there's no way to
   tag a release that skips this check.
2. **`Sign artifacts with cosign (keyless)` step** — runs under
   `set -euo pipefail`. A sign failure aborts the job, so an
   unsigned release cannot ship. The matching client-side
   verification was added in v0.6.44.

Together: schema break → tag push → workflow fires →
`verify-update-cycle` fails → matrix never starts → no release.
Schema break + bypassed local tests + manually-uploaded artifacts
is the only path to a broken release, and that path requires the
maintainer to consciously skip the workflow.

## Anti-patterns to avoid

- **Don't run the full `cargo test` suite locally** — only the
  modules you touched. Full suite freezes the test machine.
- **Don't auto-add features.** If a "while you're there" temptation
  surfaces, capture it as a separate plan entry rather than
  bundling.
- **Don't break security defaults.** Never lower `autonomy=full` or
  remove approval gates. If a feature genuinely needs more
  permission, document it in the commit body.
- **Don't bundle unrelated fmt drift into feature commits.** Run
  `cargo fmt --all` as its own `style:` commit when drift
  accumulates; keep feature commits focused.
- **Don't reproduce session-specific tooling assumptions in commit
  messages or docs.** Keep these conventions terminal-agnostic so a
  future agent can pick them up cold.
