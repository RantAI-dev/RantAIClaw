# Plan 284: `ui install --dir` must not delete a directory just because it holds `.git`

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the row in `plans/280-production-readiness-handoff.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0dd4c03..HEAD -- src/webui.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P0 — BLOCKER (ledger W0-5), data loss
- **Effort**: S
- **Risk**: HIGH if done carelessly — the code under change is a recursive delete
- **Depends on**: nothing
- **Category**: bug / data safety
- **Planned at**: commit `0dd4c03`, 2026-09-04

## Why this matters

`ui install` treats "contains a `.git` directory" as proof that it owns the target, and an
owned target skips the `--force` guard and is recursively deleted before extraction. Point
`--dir` at a dotfiles checkout, a clone, or any repository and it is gone. There is no
prompt, no backup, and nothing about the flag's name suggests deletion.

The `.git` heuristic exists for a real reason — an earlier installer used `git clone` — but
it identifies *any* repository, not *this installer's* leftovers.

## Current state (verified at `0dd4c03`)

```rust
// src/webui.rs:576-588
// Refuse to clobber a non-empty dir that we did not create, unless --force.
// `.git` covers a directory left over from the previous git-clone-based installer.
let managed = dir.join("server.js").exists() || dir.join(".git").is_dir();
let non_empty = dir.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false);
if non_empty && !managed && !force {
    bail!("{} exists and is not empty — pass --force to overwrite", dir.display());
}
```

```rust
// src/webui.rs:638-642
// Verified: safe to extract. Wipe any prior layout (managed dir).
if dir.exists() {
    std::fs::remove_dir_all(&dir).with_context(|| format!("clear {}", dir.display()))?;
}
```

`managed == true` therefore both skips the guard and licenses the wipe.

## Steps

1. **Narrow "managed" to something this installer actually writes.** Keep
   `server.js` (the standalone entrypoint the installer extracts). Replace the bare `.git`
   test with a marker that only a claw-ui install produces — the version file the installer
   already writes, or a `.git` directory *whose origin remote is the claw-ui repository*.
   Prefer the marker file: it is a plain `exists()` check with no git dependency.
   **Verify**: read the extraction code below `:640` and confirm which marker files the
   installer itself creates; use one of those. If none exists, create one during install
   and treat its absence as unmanaged.

2. **Make the destructive step defensive independently of the guard.** Before
   `remove_dir_all`, re-assert that the directory looks like a claw-ui install (the same
   marker) or that `force` was passed. Two independent checks, because this is a recursive
   delete and the audit found one path around the first check already.
   **Verify**: `rg -n 'remove_dir_all' src/webui.rs` — every occurrence is preceded by a
   check in the same function.

3. **Cover the loss case.** Add a test: a temp directory containing only `.git/` plus an
   unrelated file, `force = false` → install refuses with the "pass --force" error and the
   temp directory is **still intact** afterwards. Assert on the surviving file, not just on
   the error, so the test proves nothing was deleted.
   **Verify**: `cargo test --lib webui` passes; the test fails if step 1 is reverted.

4. **Keep the legacy path working.** A genuine leftover from the old git-clone installer
   (a `.git` directory *and* claw-ui content) must still upgrade cleanly. Add that as a
   second test.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib webui` passes with both new tests.
- A directory holding only `.git` and unrelated files survives `ui install --dir` without
  `--force`.

## STOP conditions

- The extraction code writes no marker file and adding one changes the release archive
  contract → STOP and report; that needs a claw-ui-side decision.
- Any step would make `remove_dir_all` run in a path with fewer checks than today → STOP.

## Test plan

Two tests in `webui.rs`'s test module using `tempfile::TempDir`. Never point a test at a
path outside the temp directory.

## Maintenance note

`remove_dir_all` on an operator-supplied path is the highest-consequence line in this file.
Any future edit near it deserves the same two-independent-checks treatment.

## Rollback

One commit, one file plus tests. Reverting restores the prior (dangerous) behaviour, so
prefer fixing forward.
