# Plan 238: Tighten file modes on secret stores

> **Executor instructions**: Follow step by step; verify each step; STOP-condition = stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/auth/profiles.rs src/config/schema.rs src/service/mod.rs`
> Mismatch against the excerpts = STOP.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW (mode tightening + a chown flag; existing installs on default setups resolve identically)
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

Three files that hold or shelter credentials are written under the process umask (typically 0644 / 0755) rather than 0600, so any local account can read them for a window or permanently:

1. `auth-profiles.json` — OAuth access + refresh tokens. Written with a plain `fs::write` on the temp file, no mode. Under a system install the OpenRC installer never chmods it, so it sits world-readable in `/etc/rantaiclaw`.
2. `config.toml` — its temp file gets 0600 AFTER the write, not at open, so there is a brief world-readable window on every save/PUT (the file holds channel tokens, `password_hash`, `paired_tokens`, and — with `secrets.encrypt=false` — plaintext provider keys).
3. Installer `chown -R` lacks `--no-dereference`; on BusyBox/Alpine it follows symlinks, a local priv-esc primitive if the daemon account is already compromised.

The correct pattern already exists in `src/security/secrets.rs` — mode-at-open.

## Current state

- Exemplar (`src/security/secrets.rs:182-197`) — copy this shape:
  ```rust
  #[cfg(unix)]
  {
      use std::io::Write;
      use std::os::unix::fs::OpenOptionsExt;
      let mut f = std::fs::OpenOptions::new()
          .write(true).create_new(true).mode(0o600)
          .open(&self.key_path)...;
      f.write_all(...)?;
  }
  #[cfg(not(unix))] { fs::write(...)?; }
  ```
- `src/auth/profiles.rs` — `write_persisted_locked` (`:400-435`):
  ```rust
  let tmp_path = self.path.with_file_name(tmp_name);   // :418
  fs::write(&tmp_path, &json).await...;                // :420 — async fs::write, no mode
  fs::rename(&tmp_path, &self.path).await...;          // :427
  ```
  (`fs` here is `tokio::fs`.) No `set_permissions`, no `.mode()`.
- `src/config/schema.rs` — the `save()` temp write (~`:4598-4634`): `OpenOptions::new().create_new(true).write(true)` with NO `.mode()`, then writes + fsyncs, then `set_permissions(&temp_path, 0o600)` AFTER. Move the mode to open time.
- `src/service/mod.rs` — `chown_recursive_to_rantaiclaw` (`:807-824`):
  ```rust
  let output = Command::new("chown")
      .args(["-R", "rantaiclaw:rantaiclaw", &path.to_string_lossy()])   // :810 — no -h/-P
      .output()...;
  ```
  Also add `auth-profiles.json` to the OpenRC installer's explicit chmod list (find where `config.toml`/`.secret_key` are chmod'd, ~`service/mod.rs:1109-1126`).

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib auth::profiles` | pass |
| Test | `cargo test --lib config::schema` | pass |
| Test | `cargo test --lib service` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/auth/profiles.rs` (mode-at-open on the temp file + a mode assertion test)
- `src/config/schema.rs` (mode-at-open on the config temp file)
- `src/service/mod.rs` (`chown` gets `--no-dereference`; add auth-profiles to OpenRC chmod list)

**Out of scope**:
- The encryption logic itself (whether `secrets.encrypt` is on) — unchanged.
- `src/security/secrets.rs` — it is the exemplar, already correct.

## Git workflow

- Branch: `fix/secret-file-mode-hardening`
- Message e.g. `fix(security): create secret stores 0600 at open and stop chown following symlinks`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Write `auth-profiles.json` temp file 0600 at open

In `write_persisted_locked` (`profiles.rs:420`), replace the `tokio::fs::write(&tmp_path, &json)` with a `#[cfg(unix)]` branch that opens the temp file via `std::fs::OpenOptions` with `.create_new(true).write(true).mode(0o600)` and writes the bytes (blocking write inside `spawn_blocking`, or `tokio::fs::OpenOptions` with `OpenOptionsExt` if available), and a `#[cfg(not(unix))]` fallback that keeps the current write. Keep the temp→rename flow.

**Verify**: `cargo test --lib auth::profiles` compiles + passes; Test-plan `auth_store_is_0600` passes.

### Step 2: Move the config temp file's mode to open time

In `Config::save`'s temp write (`schema.rs:~4598`), add `.mode(0o600)` to the `OpenOptions` chain under `#[cfg(unix)]`. Keep the existing post-write `set_permissions` as a belt-and-braces (the existing `config_permissions_are_relocked_on_resave` test at `schema.rs:~7543` still covers the end state).

**Verify**: `cargo test --lib config::schema` → pass (including the existing permissions test).

### Step 3: Stop `chown` following symlinks; chmod the auth store on OpenRC install

In `chown_recursive_to_rantaiclaw` (`service/mod.rs:810`), add `-h` (or `--no-dereference`) to the args; prefer an in-process walk using `symlink_metadata` + `std::os::unix::fs::lchown` if straightforward, else the flag. In the OpenRC installer's chmod block (~`:1109-1126`), add `auth-profiles.json` to the files chmod'd to 0600.

**Verify**: `cargo test --lib service` → pass.

## Test plan

- `auth::profiles`: `auth_store_is_0600` — write a store, assert the on-disk file mode is `0o600` (Unix; mirror `schema.rs`'s `new_config_file_has_restricted_permissions` test if present). Also assert the tmp file left no `.tmp.*` residue.
- `config::schema`: rely on the existing permissions test; add `config_temp_is_created_0600` only if a seam allows observing the temp file mode (skip if it races the rename).
- Verification: `cargo test --lib auth::profiles config::schema service` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped auth::profiles + config::schema + service tests pass, with `auth_store_is_0600`
- [ ] `grep -n "\"-R\", \"rantaiclaw" src/service/mod.rs` shows `-h`/`--no-dereference` present (or an lchown walk replaces the shell-out)
- [ ] `git status` shows only the three in-scope files
- [ ] `plans/README.md` row updated

## STOP conditions

- `tokio::fs` cannot set mode-at-open and there is no clean `spawn_blocking` path — report; do not fall back to chmod-after-write for the auth store (that reintroduces the window).
- The OpenRC chmod block isn't found near the cited lines (drift) — report.
- Adding `-h` to a BusyBox `chown` that doesn't support it errors — confirm the flag is portable on the target; if not, use the `lchown` walk.

## Maintenance notes

- Reviewer: confirm the auth store test asserts the actual on-disk mode (not just existence) and that the config temp is 0600 at open, not only after.
- Rotation: any OAuth token or provider key that was written world-readable on an affected system install should be treated as exposed and rotated.
- `secrets.encrypt=false` still stores plaintext tokens; the mode hardening is defense-in-depth, not a substitute for encryption.
