# Plan 002: Clear the environment before spawning MCP subprocesses

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/mcp/`
> If any file under `src/mcp/` changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

MCP server subprocesses are spawned with `.envs(env)` and **no `.env_clear()`**,
so each child inherits the daemon's entire process environment — provider API
keys, proxy credentials (which can embed `user:pass@`), and anything else
exported into the daemon — on top of its own declared `env` map. MCP servers are
frequently third-party npm/uv packages the operator did not write; a single
compromised or malicious MCP dependency can read and exfiltrate every daemon
secret with no extra access. The shell tool already hardens exactly this way
(`env_clear()` + a `SAFE_ENV_VARS` allowlist); the MCP path never adopted it.
This closes a privileged-boundary secret leak into untrusted subprocesses.

## Current state

- `src/mcp/client.rs` — one-shot MCP client (`connect`). Spawn at lines 71-79:
  ```rust
  let mut child = Command::new(command)
      .args(args)
      .envs(env)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .spawn()
      .with_context(|| format!("spawn MCP server `{server_name}` ({command})"))?;
  ```

- `src/mcp/handle.rs` — supervised handle. `spawn` at lines 36-46 and `respawn`
  at lines 66-75 both use the same `.args(&args).envs(&env)...spawn()` pattern
  with no `env_clear`.

- **The pattern to copy** — `src/tools/shell.rs:430-436` (verified):
  ```rust
  cmd.env_clear();
  for var in SAFE_ENV_VARS {
      if let Ok(val) = std::env::var(var) {
          cmd.env(var, val);
      }
  }
  ```
  `SAFE_ENV_VARS` is defined in the security policy layer. Grep for it:
  `grep -rn "SAFE_ENV_VARS" src/` — confirm the exact path and contents before
  reusing (it lives near `src/security/policy.rs`). It is an allowlist of
  non-secret vars (PATH, HOME, locale, etc.).

- Why the daemon env carries secrets (context, do not need to change): proxy
  URLs may be written into `HTTP_PROXY`/`http_proxy` (`src/config/schema.rs`
  around 1654), and provider/skill code transiently sets keys via
  `std::env::set_var`. Provider API keys are also conventionally exported in the
  launching shell.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| MCP tests | `cargo test mcp` | all pass |
| Lib tests (2 crate roots) | `cargo test --lib mcp` | all pass |

## Scope

**In scope**:
- `src/mcp/client.rs`
- `src/mcp/handle.rs`
- A new test module (inline `#[cfg(test)]` in `src/mcp/handle.rs`, or a
  `tests/mcp_env_isolation.rs` integration test — see Test plan)

**Out of scope** (do NOT touch):
- `SAFE_ENV_VARS` itself (`src/security/`) — reuse it, do not edit it. If it is
  private to that module, add a `pub(crate)` re-export in the smallest possible
  scope rather than duplicating the list.
- The MCP JSON-RPC protocol / handshake code.
- The configured `env` map semantics — the operator's explicitly-declared
  `env` entries must STILL be passed (they are the whole point).

## Git workflow

- Branch: `advisor/002-mcp-subprocess-env-clear`
- One commit; message e.g.
  `security(mcp): env_clear before spawning subprocesses; pass only allowlist + configured env`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Add a shared spawn-env helper

To avoid three divergent copies (`connect`, `spawn`, `respawn`), add one small
helper that applies the hardening to a `tokio::process::Command`. Put it in
`src/mcp/mod.rs` (or wherever the MCP module root is — check `src/mcp/`), e.g.:

```rust
/// Strip the inherited process environment and re-add only a non-secret
/// allowlist plus the explicitly-configured `env` map, mirroring the shell
/// tool's hardening (`src/tools/shell.rs`). Prevents leaking daemon secrets
/// (provider keys, proxy credentials) into third-party MCP subprocesses.
pub(crate) fn apply_hardened_env(cmd: &mut tokio::process::Command, env: &std::collections::HashMap<String, String>) {
    cmd.env_clear();
    for var in crate::security::policy::SAFE_ENV_VARS {   // adjust path to the real one
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    cmd.envs(env);   // configured env overrides allowlist — intentional
}
```

Resolve the real `SAFE_ENV_VARS` path from the grep in Current state. If it is
not reachable as `pub(crate)`, make it so with the minimal visibility change.

**Verify**: `cargo build 2>&1 | tail -5` → compiles (the helper is unused until
step 2, so expect an unused-function warning; that is fine mid-step).

### Step 2: Route all three spawn sites through the helper

In `src/mcp/client.rs` `connect` and `src/mcp/handle.rs` `spawn`/`respawn`,
replace the `.envs(env)` builder call with the helper. Because the helper takes
`&mut Command`, restructure each from the fluent chain to:

```rust
let mut cmd = Command::new(command);
cmd.args(args);
crate::mcp::apply_hardened_env(&mut cmd, env);
cmd.stdin(Stdio::piped())
   .stdout(Stdio::piped())
   .stderr(Stdio::piped())
   .kill_on_drop(true);
let mut child = cmd.spawn().with_context(...)?;
```

Do this at all three sites. Keep every other builder option identical
(`kill_on_drop`, the three piped stdio handles).

**Verify**: `cargo build 2>&1 | tail -5` → compiles, no unused-function warning.

### Step 3: Confirm no remaining bare `.envs(` spawn in MCP

**Verify**: `grep -rn "\.envs(" src/mcp/` → every hit is inside
`apply_hardened_env` (or none). No spawn site calls `.envs(...)` directly.

## Test plan

- New test (prefer inline `#[cfg(test)]` in `src/mcp/handle.rs`): set a fake
  secret var in the test process (e.g. `std::env::set_var("RANTAICLAW_TEST_SECRET", "leak")`),
  spawn a trivial child through the hardened path that echoes its environment
  (e.g. `sh -c 'env'` on Unix), capture stdout, and assert the output does NOT
  contain `RANTAICLAW_TEST_SECRET` but DOES contain a configured `env` entry you
  passed and an allowlisted var like `PATH`.
  - This test mutates process-global env: acquire `crate::test_env::ENV_LOCK`
    (see `src/test_env.rs`) — `blocking_lock()` for a sync test,
    `.lock().await` for `#[tokio::test]`. Restore/remove the var at the end.
  - Model the structure after an existing MCP test: `grep -rn "#\[tokio::test\]" src/mcp/`.
- Verification: `cargo test mcp` → all pass including the new isolation test.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `grep -rn "\.envs(" src/mcp/` shows no direct spawn-site call (only the helper)
- [ ] `cargo test mcp` passes; the new env-isolation test exists and asserts the fake secret is absent from the child env
- [ ] No files outside the in-scope list modified except a minimal visibility change to expose `SAFE_ENV_VARS` (note it in the PR)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- `SAFE_ENV_VARS` does not exist or is not a simple list of var names (the grep
  returns something unexpected) — report what you found before inventing a list.
- Any curated MCP server in `src/mcp/` documents a *required* inherited var not
  in `SAFE_ENV_VARS` (e.g. `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`) — do not just
  drop it; report so the allowlist decision is made deliberately.
- The spawn code at the cited lines does not match the excerpts.

## Maintenance notes

- Rotate any provider/proxy credentials that were previously exposed to
  third-party MCP servers — a leaked secret is burned even after this fix. State
  this in the PR body (do not put any secret value in the PR).
- If a future MCP server legitimately needs an extra inherited var, add it to
  the shared allowlist (or a dedicated MCP passthrough list) — never re-introduce
  a blanket `.envs()` without `env_clear()`.
- Reviewer should scrutinize that the configured `env` map still reaches the
  child (a regression here silently breaks every MCP server that needs its keys).
