# Plan 006: Create the secret master-key file with mode 0600 atomically

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/security/secrets.rs`
> If the file changed since this plan was written, compare the "Current state"
> excerpt against the live code; on a mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

The 256-bit ChaCha20-Poly1305 master key that decrypts every stored secret
(`enc2:` values in config) is written with `fs::write` (creating the file under
the process umask — commonly group/world-readable) and only *afterward* chmod'd
to 0600. Between the write and the chmod there is a window where the master key
is readable by other local users on a shared host. Anyone who reads it in that
window can decrypt all persisted secrets. The window is narrow and local, but
the blast radius (the master key) is maximal. Creating the file with 0600 at
open time closes it.

## Current state

- `src/security/secrets.rs:170-190` — `load_or_create_key`:
  ```rust
  fn load_or_create_key(&self) -> Result<Vec<u8>> {
      if self.key_path.exists() {
          let hex_key = fs::read_to_string(&self.key_path).context("Failed to read secret key file")?;
          hex_decode(hex_key.trim()).context("Secret key file is corrupt")
      } else {
          let key = generate_random_key();
          if let Some(parent) = self.key_path.parent() {
              fs::create_dir_all(parent)?;                       // line 178-179: default dir perms
          }
          fs::write(&self.key_path, hex_encode(&key))            // line 181: umask perms, THEN chmod
              .context("Failed to write secret key file")?;
          #[cfg(unix)]
          {
              use std::os::unix::fs::PermissionsExt;
              fs::set_permissions(&self.key_path, fs::Permissions::from_mode(0o600))  // line 188
                  .context("Failed to set key file permissions")?;
          }
          #[cfg(windows)]
          { /* icacls path — keep as-is */ }
          ...
  }
  ```
  (Read the full function to line ~210 to see the Windows branch and the return.)

- Repo convention: Unix-specific behavior is `#[cfg(unix)]`-gated; Windows uses
  `icacls`. `std::os::unix::fs::OpenOptionsExt::mode` is the idiomatic way to set
  create-time mode. `libc` is already a Unix dependency (`Cargo.toml:238-239`).

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Secrets tests | `cargo test secrets` | all pass, incl. new |

## Scope

**In scope**:
- `src/security/secrets.rs` — the key-file creation (Unix branch) and,
  optionally, tightening the parent-dir mode to 0700.
- New test in the same file's `#[cfg(test)]` module.

**Out of scope** (do NOT touch):
- The Windows `icacls` branch — leave it as-is (only note it in the PR).
- The encryption/decryption logic, key generation, or the `enc2:` format.
- Reading of an existing key (the `if self.key_path.exists()` branch).

## Git workflow

- Branch: `advisor/006-secret-key-file-mode-atomic`
- One commit; message e.g.
  `security(secrets): create master-key file with 0600 at open time (close world-readable window)`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Create the key file with 0600 at open time (Unix)

Replace the `fs::write(...)` + later `fs::set_permissions(...)` sequence, on
Unix, with a single create-with-mode write:

```rust
#[cfg(unix)]
{
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)          // fail if it already exists (we only reach here when it doesn't)
        .mode(0o600)               // restrictive from the first byte
        .open(&self.key_path)
        .context("Failed to create secret key file")?;
    f.write_all(hex_encode(&key).as_bytes())
        .context("Failed to write secret key file")?;
}
#[cfg(not(unix))]
{
    fs::write(&self.key_path, hex_encode(&key))
        .context("Failed to write secret key file")?;
}
```

Then **remove** the now-redundant `#[cfg(unix)]` `fs::set_permissions(0o600)`
block (lines 185-190) — the mode is already set at creation. Keep the
`#[cfg(windows)]` `icacls` block unchanged (it still runs after the
`#[cfg(not(unix))]` write on Windows).

Note: `create_new(true)` will error if the file exists. That is correct here —
this branch is only entered when `self.key_path.exists()` was false. If you are
worried about a TOCTOU between the `.exists()` check and the create, that is a
*safer* failure (error out rather than overwrite a key), which is acceptable;
do not "fix" it by switching to `create(true)`.

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

### Step 2: Tighten the parent directory to 0700 (Unix)

After `fs::create_dir_all(parent)?` (line 179), on Unix set the parent dir mode
to 0700 (the key dir should not be group/world-traversable). Use
`fs::set_permissions(parent, Permissions::from_mode(0o700))` guarded by
`#[cfg(unix)]`, and only if `parent` was newly created — or unconditionally if
the dir is a dedicated secrets dir (read the surrounding code to confirm the dir
is RantaiClaw-owned, not a shared config dir; if it is shared, SKIP this step and
note it, to avoid tightening a directory the user expects to be readable).

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

## Test plan

- New unit test in `src/security/secrets.rs` `#[cfg(test)]` (Unix-gated):
  1. `master_key_file_created_0600`: point the store at a temp dir (use
     `tempfile::TempDir`), trigger key creation (call `load_or_create_key` or the
     public entry that creates it), then read the file mode via
     `std::fs::metadata(path)?.permissions().mode() & 0o777` and assert it equals
     `0o600`.
  2. (If Step 2 applied) `master_key_dir_created_0700`: assert the parent dir
     mode is `0o700`.
  - Gate the test with `#[cfg(unix)]`. If key creation reads/writes process env
    or a global config dir, acquire `crate::test_env::ENV_LOCK` (see
    `src/test_env.rs`).
  - Model after an existing secrets test: `grep -n "#\[test\]\|#\[cfg(test)\]" src/security/secrets.rs`.
- Verification: `cargo test secrets` → all pass including the new test(s).

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `grep -n "set_permissions" src/security/secrets.rs` shows the post-write
      0600 chmod on the key file is gone (dir-perms tightening, if added, may
      remain)
- [ ] `cargo test secrets` passes; the new 0600-at-creation test exists (Unix-gated)
- [ ] Only `src/security/secrets.rs` modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The key-creation code does not match the excerpt (drift since `4d35107`).
- The key's parent directory turns out to be a shared config dir users expect to
  read — then skip Step 2 and report, rather than tightening it.

## Maintenance notes

- Existing installs may already have a world-readable key file created before
  this fix. Add a note in the PR body recommending operators regenerate the key
  (which forces re-encryption of stored secrets = rotation). Do NOT print any
  key value anywhere.
- Reviewer should confirm `create_new` cannot regress to silently overwriting an
  existing key, and that the Windows path still restricts via `icacls`.
