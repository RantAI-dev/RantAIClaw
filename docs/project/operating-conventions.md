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
