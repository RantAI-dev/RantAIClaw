# Plan 234: Constrain the config-API MCP-server write path

> **Executor instructions**: Follow step by step; verify each step; on any "STOP
> condition" stop and report. Update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 0e5fcc9..HEAD -- src/gateway/config_api.rs src/mcp/mod.rs`
> Mismatch against the excerpts below = STOP.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: HIGH (this route can spawn local processes; changing its contract affects existing consoles)
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `0e5fcc9`, 2026-08-27

## Why this matters

`POST /api/v1/config/mcp_servers/{name}` accepts `command`, `args`, `env` from any authenticated (paired) client, validates only "name non-empty" and "command non-empty", persists it, and the agent then spawns `Command::new(command)` on construction. So a chat pairing token converts into arbitrary, persistent local process execution as the daemon user — no approval, no audit, no cap. On a `require_pairing = false` deployment it is reachable by anyone who can reach the port. Two independent defects on the same route:

1. **Ungated exec** (security): no command validation, no per-write cap (the runtime registry caps at `MAX_MCP_SERVERS = 10` but the config write does not), and caller-supplied `env` overrides the hardened loader env.
2. **Silent full-replace** (data loss): re-adding an existing name replaces the whole record, wiping its stored `env` and `args`. The panel's `add()` never sends `env`, so re-adding to fix a typo destroys every API key in that server's env.

After this lands: the write path validates the command, caps the entry count, refuses loader-influencing env keys, and MERGES onto an existing entry instead of destroying it.

## Current state

- `src/gateway/config_api.rs`:
  ```rust
  #[derive(Deserialize)]                                  // :425
  struct McpServerBody { command: String,
      #[serde(default)] args: Vec<String>,
      #[serde(default)] env: HashMap<String, String> }
  async fn add_mcp_server(...) -> ... {                   // :434
      check_auth(&state, &headers)?;                      // :440
      let name = name.trim().to_string();
      if name.is_empty() { return Err(err_400("server name must not be empty")); }
      if body.command.trim().is_empty() { return Err(err_400("command must not be empty")); }
      let (_guard, mut cfg) = lock_and_load().await?;
      cfg.mcp_servers.insert(name.clone(), McpServerConfig {   // :449 — full replace
          command: body.command.trim().to_string(), args: body.args, env: body.env });
      let count = cfg.mcp_servers.len();
      persist_and_swap(&state, cfg).await?; ...
  }
  ```
- Exemplar of proper validation on the SAME router: the `allowed_commands` branch of `set_autonomy` (`config_api.rs:369-381`) runs each entry through `crate::approval::permissions::validate_allow_basename` and returns `err_400` on a bad value. Match that shape.
- `src/mcp/mod.rs`: `const MAX_MCP_SERVERS: usize = 10;` (`:20`); `apply_hardened_env` (`:30`) clears inherited env then applies the caller `env` on top (documented "configured env overrides the allowlist — intentional"); the registry enforces the cap at `:52-56` but the config write does not.
- `src/mcp/curated.rs` exists — inspect it for a curated/known-server list to validate against (open it before Step 2).

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Clippy | `cargo clippy --lib -- -D warnings` | exit 0 |
| Test | `cargo test --lib gateway::config_api` | pass |
| Test | `cargo test --test config_api` | pass |

**Disk constraint**: never bare `cargo test`.

## Scope

**In scope**:
- `src/gateway/config_api.rs` (`McpServerBody`, `add_mcp_server`, new tests)

**Out of scope**:
- `src/mcp/mod.rs` — read `MAX_MCP_SERVERS` / `apply_hardened_env` only; do not change the runtime registry.
- The MCP READ redaction — that is plan 232.
- The claw-ui mcp-panel — a claw-ui PR handles the client-side collision confirm.

## Git workflow

- Branch: `fix/mcp-server-api-hardening`
- Message e.g. `fix(security): validate, cap, and merge the config-API MCP server write path`
- Do NOT push/PR unless instructed.

## Steps

### Step 1: Make `env`/`args` optional so omission means "keep existing"

Change `McpServerBody.env` and `.args` to `Option<...>` (`config_api.rs:426-431`). This matches the "provided sets, omitted keeps" contract already used by `apply_secrets` and `set_knowledge` elsewhere in this file.

**Verify**: `cargo build --lib` compiles (fix the call site in Step 3 first if needed).

### Step 2: Validate the command and cap the entry count

In `add_mcp_server`, after the name/command non-empty checks: reject a `command` containing shell metacharacters (`;`, `|`, `&`, backtick, `$(`), resolve it to an absolute path or a bare basename (no path traversal), and refuse `env` keys that influence the dynamic loader (`LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_*`). If `cfg.mcp_servers` does not already contain `name` and `cfg.mcp_servers.len() >= 10`, return `err_400("too many MCP servers (max 10)")`. Return `err_400` with a clear message on each rejection (mirror the `set_autonomy` allowed_commands branch).

**Verify**: `cargo test --lib gateway::config_api` compiles + passes.

### Step 3: Merge onto the existing entry instead of replacing

Replace the `cfg.mcp_servers.insert(...)` full-replace at `:449` with a merge: look up the existing `McpServerConfig` for `name`; set `command` from the body (it is required); for `args`/`env` use the body value when `Some`, else keep the existing entry's value. Insert the merged record.

**Verify**: the Test-plan `readd_preserves_existing_env` test passes.

## Test plan

Model after the `#[cfg(test)]` block in `config_api.rs` and `tests/config_api.rs` (has `spawn_test_gateway`):
- `add_mcp_server_rejects_shell_metacharacters` — a `command` with `;`/`|` → 400.
- `add_mcp_server_rejects_loader_env` — `env` with `LD_PRELOAD` → 400.
- `add_mcp_server_caps_at_ten` — 11th distinct name → 400; an update to an existing name at the cap → OK.
- `readd_preserves_existing_env` — POST name X with env {A:secret}; POST name X again with only a new command and no env; assert the stored entry still has env {A:secret}. Use a local marker for the secret value.
- Verification: `cargo test --lib gateway::config_api` and `cargo test --test config_api` → all pass.

## Done criteria

- [ ] `cargo fmt --all -- --check` exit 0; `cargo clippy --lib -- -D warnings` exit 0
- [ ] scoped config_api tests pass with the 4 new tests
- [ ] `grep -n "cfg.mcp_servers.insert" src/gateway/config_api.rs` shows the merge, not a bare full-replace of body fields
- [ ] `git status` shows only `src/gateway/config_api.rs`
- [ ] `plans/README.md` row updated

## STOP conditions

- `validate_allow_basename` or `MAX_MCP_SERVERS` no longer exist (drift) — STOP.
- Curated-list validation would break the ability to register a legitimate non-curated local server AND there is no operator allowlist config to gate it — in that case implement only the metachar/loader/cap checks + merge (still a strict improvement) and note the curated-allowlist as deferred; do NOT hard-block all non-curated servers without an opt-in.
- Change requires editing `src/mcp/` — out of scope; report.

## Maintenance notes

- Reviewer: confirm the merge preserves `env` on re-add (the data-loss half) AND that at least one test proves a dangerous command/env is rejected (the security half).
- This route is privileged, not ordinary config. If an audit-trail facility lands (plan 237), emit a record here.
- Rotation: any MCP `env` secret destroyed by a pre-fix re-add must be re-issued, not just re-entered, if the wipe went unnoticed.
