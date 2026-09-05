# Plan 215 (SPIKE): The `[security.sandbox]` layer is entirely dead — decide to wire one real backend or delete it

> **Executor instructions**: This is a DESIGN/DECISION spike, not a mechanical
> fix. Produce the decision + a follow-up plan (or the deletion) described
> below. Do not half-wire a sandbox. On a "STOP condition", stop and report.
> When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/security/detect.rs src/security/traits.rs src/security/bubblewrap.rs src/security/firejail.rs src/security/landlock.rs src/security/docker.rs src/config/schema.rs src/tools/shell.rs src/runtime/native.rs`

## Status

- **Priority**: P1 (security — a configured control that does nothing)
- **Effort**: L (multi-day if wiring; the spike itself is S–M)
- **Risk**: HIGH (wiring interacts with env-clear, process-group kill, timeout)
- **Depends on**: none
- **Category**: security / direction (spike)
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

The entire `[security.sandbox]` backend layer is **dead code**. `create_sandbox`
has **zero** production callers; `Sandbox::wrap_command` is called only in
`src/security/*` tests. The shell tool builds its `Command` and calls
`cmd.spawn()` directly (`src/tools/shell.rs:459` → `src/runtime/native.rs:62`),
and MCP subprocesses spawn directly too. So an operator who sets
`backend = "bubblewrap"` (or `landlock`/`firejail`/`docker`) gets **exactly
zero** OS-level confinement — the knob is a security-relevant no-op.

Worse, the config can't even be set: `SecurityConfig` is **not a field of
`Config`** (there is no `deny_unknown_fields`), so `[security.sandbox]` in
`config.toml` is silently dropped and absent from `config schema` output. And
each backend is independently defective if it were ever wired:

- `create_sandbox` **fails open** to `NoopSandbox` (warn-only) when a requested
  backend is unavailable.
- `bubblewrap` binds host `/tmp` read-write (leak) and omits `/bin`/`/lib`/`/etc`
  (broken for real binaries); the workspace is never bound; no `--clearenv`.
- `landlock` restricts the **current** (daemon) process, not the child, and
  never adds the workspace to the ruleset — wiring it naively would brick the
  agent on the first command.
- `docker` here is a dead duplicate of the working `[runtime].kind = "docker"`
  path (`src/runtime/docker.rs`), which is the one that actually confines.
- `wrap_command` rebuilds the `Command` from program+args only, silently
  dropping env / cwd / `pre_exec` / stdio / `kill_on_drop` — so any correct
  wiring must first redesign that contract.

There IS a working confinement path today: `[runtime].kind = "docker"` (a
different config section). Default `native` = no confinement (which, for the
shell tool, is intentional per CLAUDE.md §3.6 — but the *sandbox knob pretending
to add confinement* is not).

## The decision (produce this)

Choose ONE and record it in the PR + `plans/README.md`:

### Option A — wire ONE backend correctly (recommended: Landlock, in a child)

Deliver real OS confinement for the shell tool via a single backend, done right:

1. **Connect config.** Add `pub security: SecurityConfig` to `Config` with
   `#[serde(default)]` so `[security.*]` is settable and appears in
   `config schema`. (Coordinate with plan 216, which marks the still-dead keys.)
2. **Redesign `wrap_command`** to preserve the caller's `Command` (env, cwd,
   `pre_exec`, stdio, `kill_on_drop`) — e.g. return an argv **prefix** the
   runtime composes, or apply restrictions inside a post-fork `pre_exec` rather
   than rebuilding the `Command`.
3. **Fix the chosen backend.** For Landlock: apply the ruleset in the child via
   `pre_exec` (post-fork, pre-exec) — NOT `restrict_self` on the daemon — and
   add the real workspace dir to the ruleset. For bubblewrap: `--tmpfs /tmp`,
   bind the standard ro system dirs, bind the workspace, `--clearenv` + explicit
   setenv, `--chdir`.
4. **Fail closed.** When an explicitly-requested backend is unavailable, refuse
   to start (or hard-block the shell tool), not silent Noop.
5. **Wire it** into `shell.rs` before `spawn()` (and consider MCP subprocess
   spawns). Keep `native` = no sandbox as the default.

This is L-effort and HIGH-risk; scope it as its own follow-up plan (this spike
produces that plan).

### Option B — delete the dead layer, document `[runtime].kind` as the control

If real sandboxing is not a near-term goal:

1. Delete `src/security/{bubblewrap,firejail,landlock,docker}.rs`,
   `detect.rs`'s `create_sandbox`, the `Sandbox` trait, and the `[security.sandbox]`
   config (or keep the config only if 216 keeps a "roadmap" marker).
2. Correct `src/security/traits.rs:6-7` and `docs/security/*` which assert the
   sandbox is "applied before every shell execution" — it never was.
3. Document `[runtime].kind = "docker"` as the actual confinement control.

Option B removes a security *lie* immediately; Option A removes it by making the
control real. Do **not** leave the layer half-wired.

## Deliverable of this spike

- A written decision (A or B) with rationale, in the PR description and the
  `plans/README.md` row.
- If A: a new follow-up implementation plan (e.g. `plans/2xx-landlock-child-wiring.md`)
  covering steps A.1–A.5 with the `wrap_command` redesign and per-backend fixes,
  and its own test matrix (a real command is confined; a forbidden read is
  denied; the daemon is not restricted).
- If B: the deletion PR + the doc corrections (this can be executed directly).

## Files (for the eventual implementation, not the spike)

- Config: `src/config/schema.rs` (SecurityConfig→Config).
- Sandbox: `src/security/{detect,traits,bubblewrap,firejail,landlock,docker}.rs`.
- Wiring: `src/tools/shell.rs`, `src/runtime/native.rs`, possibly `src/mcp/*`.
- Docs: `src/security/traits.rs` doc comments, `docs/security/*` (also plan 216).

## STOP conditions

- Do not wire a backend without first redesigning `wrap_command` — rebuilding the
  `Command` drops the env-clear, process-group `pre_exec`, and stdio the shell
  tool depends on, breaking cancel/timeout and output capture.
- Do not wire Landlock via `restrict_self` on the daemon — it is irreversible and
  process-global; it must be applied in the child.
- If the maintainer picks B, do not also spend effort "improving" the backends
  being deleted.

## Done criteria (spike)

- The decision is recorded, and either the follow-up plan (A) exists or the
  deletion+doc-correction PR (B) is opened. No source behavior change is required
  *by this spike itself* beyond (for B) the deletion.

## Risk & rollback

- The spike itself is low-risk (a decision + a plan or a deletion). The eventual
  Option A implementation is HIGH-risk and must land behind its own tests.

## Maintenance note

Whatever is decided, `src/security/traits.rs` and `docs/security/*` must stop
asserting a sandbox is applied when it is not (plan 216 handles the immediate
labeling regardless of A/B). A control that is configured but unwired is the
worst state; this spike exists to leave that state.
