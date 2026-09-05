# Plan 231: Direction — expose session fork, run title backfill on the gateway, warn on an exposed-but-unauthenticated bind, and give the web console a pillar doc

> **Executor instructions**: This is a **direction / feature** plan, not a bug
> fix. It bundles four grounded next-steps for the console surface that are all
> in the RantAIClaw repo. **Each of the four is independently shippable**; a
> maintainer may accept some and drop others. Confirm with the operator which
> of the four are in scope before starting if you are unsure — but the default
> is: build all four. Follow the steps, run every verification, honor STOP
> conditions, and update `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat 8503328..HEAD -- src/gateway/api_v1.rs src/gateway/mod.rs src/sessions/store.rs src/tui/app.rs docs/pillars/ README.md`

## Status

- **Priority**: P3 (direction — maintainer's call on scope)
- **Effort**: M (four small features)
- **Risk**: LOW–MED (the fork route exposes an existing store primitive; the bind warning is print-only; the pillar doc is docs)
- **Depends on**: fork route reuses `SessionStore::split_session` — **plan 221 must NOT have deleted it** (221 explicitly keeps it for this plan). No other dependency.
- **Category**: direction
- **Planned at**: commit `8503328`, 2026-08-24
- **Branch**: `feat/session-fork-and-console-ops`
- **One PR**: commit per step; the four features are separable, so a reviewer can drop a commit.

## Why this matters

Four grounded observations from the console audit, each with repo evidence:

1. **Sessions are CRUD-minus-fork.** `SessionStore::split_session` (`src/sessions/store.rs:613`) forks a session with a carried-over summary — written for compaction, wired to no route and no console. "Branch this conversation from here" is the highest-value chat feature the data model already supports.
2. **`backfill_titles` never runs on a gateway-only deployment.** Its one caller is the TUI (`src/tui/app.rs:518`), so a headless install never derives titles for legacy sessions.
3. **An exposed bind with login off gets no warning.** The gateway refuses a public bind without a tunnel or `allow_public_bind` (`src/gateway/mod.rs:866-871`), but if the operator opts into `allow_public_bind = true` (or fronts it with a tunnel) *and* has no console login configured, nothing warns that the privileged API is reachable unauthenticated.
4. **No pillar doc owns the web console.** `docs/pillars/` has nine pillars; none contains the string "console", while `README.md` makes load-bearing console claims (`:160`, `:379`, `:486`, `:583`). The cross-repo `/api/v1` contract (which drifted — see the docs findings) has no decision doc.

## Current state

### 1 — `src/sessions/store.rs:613-661`

```rust
    pub fn split_session(&self, session_id: &str, summary: &str, model: &str) -> Result<Session> {
        // ends the parent, inserts a child with parent_session_id, seeds a
        // system message = summary; returns the child Session.
```

`sessions_get` (`src/gateway/api_v1.rs:934-958`) returns `{ id, title, model, started_at, messages: [{role, content, timestamp}] }`. The router registers session routes at `api_v1.rs:41-51`.

### 2 — `src/tui/app.rs:516-518`

```rust
        // Best-effort one-shot: derive titles for legacy sessions ...
        let _ = store.backfill_titles();
```

The gateway builds its store at startup (`build_app`) and never calls this.

### 3 — `src/gateway/mod.rs:862-900`

```rust
    if is_public_bind(host) && config.tunnel.provider == "none" && !config.gateway.allow_public_bind { anyhow::bail!(...); }
    ...
    println!("🦀 RantaiClaw Gateway listening on http://{display_addr}");
```

Login-required is `config.gateway.login.password_hash.is_some()` (`mod.rs:1043`, `api_v1.rs:101`).

### 4 — `docs/pillars/1-setup.md` … `9-docs-adoption.md`; `7-gateway-daemon.md` is the closest owner (its "What this pillar covers" list, `:9-17`, does not mention the console).

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Store/handler tests | `cargo test --lib sessions::store` / `cargo test --lib api_v1::tests` | pass |
| Full lib | `cargo test --lib` | pass |
| Markdown lint (docs step) | whatever `docs` CI runs (`rtk proxy grep -rn markdownlint .github/`) | as configured |
| Never | bare `cargo test` | — disk-constrained |

## Scope

**In scope**:
- `src/gateway/api_v1.rs` (a `POST /api/v1/sessions/{id}/fork` route)
- `src/sessions/store.rs` (only if `split_session` needs a thin wrapper for a user-initiated fork — see step 1)
- `src/gateway/mod.rs` (`backfill_titles` at startup; the bind warning)
- `docs/pillars/10-web-console.md` (create), `docs/SUMMARY.md` (link it), and a short ADR under `docs/` (the cross-repo contract)

**Out of scope**:
- The claw-ui fork button + export + insights tile — **plan 232** (separate repo, separate PR).
- Changing the default bind or the login default — the warning is advisory only.
- Session export as a backend feature — `sessions_get` already returns the full transcript; export is a client-side formatter (plan 232).

## Git workflow

- Branch: `feat/session-fork-and-console-ops`.
- Commits: `feat(api): fork a session from the sessions API`, `fix(gateway): backfill session titles on startup`, `fix(gateway): warn when an exposed bind has no console login`, `docs: add a web-console pillar and the gateway/console contract ADR`.
- No `Co-Authored-By: Claude`. Do not push/PR unless instructed.

## Steps

### Step 1: `POST /api/v1/sessions/{id}/fork`

Define fork semantics first (this is the design decision `split_session` was never given for a *user* fork):

- **What the child carries**: a system message summary. For a user fork "from here", the natural summary is a short recap of the parent up to the fork point. Simplest honest v1: fork carries a system message = `"Forked from session {parent_title|id}."` plus, optionally, the last user+assistant exchange as context. Do **not** replay the whole parent (that is what a continued session already does). Keep v1 minimal: child = new session, `parent_session_id` set, one system message naming the origin. The operator continues it fresh; the parent is left intact (do NOT end the parent — `split_session` ends it, which is wrong for a user fork; see below).
- Because `split_session` **ends the parent** (`store.rs:623`), it is the wrong primitive for a non-destructive user fork. Add a sibling:
  ```rust
  /// Fork a session without ending the parent: create a new session with
  /// parent_session_id set and a single system message. The parent stays open.
  pub fn fork_session(&self, parent_id: &str, note: &str) -> Result<Session> { /* like split_session but no UPDATE ... ended_at on the parent */ }
  ```
- Handler: `sessions_fork(State, headers, Path(id), Json(body: ForkBody { note: Option<String> }))` → `check_auth`, `resolve_session_id`, `fork_session(parent, note.unwrap_or_default_recap)`, return the new session's `{ id, title, parent_session_id }`. Register the route.

Test: `fork_creates_a_child_and_leaves_the_parent_open` (store test: parent `ended_at` is still None after fork, child has `parent_session_id == parent.id`); `sessions_fork_requires_auth_when_pairing_enabled`.

**Verify**: `cargo test --lib sessions::store::tests::fork_` and `cargo test --lib api_v1::tests::sessions_fork` → pass.

### Step 2: Backfill titles on gateway startup

In `build_app` / gateway startup (wherever the store is first opened — after plan 225 that is `AppState`; before it, add a one-shot at startup in `run_gateway`), call `let _ = store.backfill_titles();` once. It is idempotent (a no-op once every session has a title).

**Verify**: no new test needed (backfill has store tests); `cargo clippy --all-targets -- -D warnings` → 0. Add a store test `backfill_titles_is_idempotent` if one does not exist.

### Step 3: Warn on exposed-but-unauthenticated bind

In `run_gateway`, after the bind (`mod.rs:876`) and near the "listening on" line (`:900`), add:

```rust
let exposed = crate::security::pairing::is_public_bind(host) || tunnel_url.is_some();
let login_off = config.gateway.login.password_hash.is_none();
if exposed && login_off {
    println!("⚠️  The gateway is reachable beyond localhost but the web console has no login configured.");
    println!("   Anyone who can reach {display_addr} can drive the agent. Set [gateway.login] or bind to 127.0.0.1.");
}
```

(This is print-only; it does not block startup — the operator opted into exposure.)

**Verify**: `cargo clippy --all-targets -- -D warnings` → 0. Manual/reasoned: the message prints only for the exposed+login-off combination. A unit test is optional (extract the predicate `fn warn_exposed_no_login(exposed: bool, login_off: bool) -> bool` and test the four combinations).

### Step 4: Web-console pillar + contract ADR

1. Create `docs/pillars/10-web-console.md` following the shape of `docs/pillars/7-gateway-daemon.md` (header with ClickUp link if the operator provides one — otherwise omit the link and note "ClickUp task TBD"; a "What this pillar covers" list: scope, the separate-repo model, cosign-verified prebuilt, the Argon2id login gate, the `/api/v1` contract, the loopback-by-default posture; a "Current state by maturity" section). Keep claims accurate to what ships (English-only, per CLAUDE.md §4.1).
2. Add it to `docs/SUMMARY.md` and the docs hub nav.
3. Create a short ADR (e.g. `docs/contributing/adr/0001-gateway-console-api-contract.md` or wherever ADRs live — `rtk proxy grep -rln 'adr\|decision' docs/` to find the convention; if none, put it under `docs/security/` or `docs/contributing/`) recording: the `/api/v1` JSON responses are the cross-repo interface between RantaiClaw and claw-ui; changes to a documented response shape are breaking and must update `docs/reference/api-v1.md` + the claw-ui types in lockstep; the console is loopback-by-default and login-optional. This is the doc whose absence let `StatusInfo`/`autonomy_preset` drift.

**Verify**: `docs` CI lint (markdown + link integrity) as configured (`rtk proxy grep -rn 'markdownlint\|link' .github/workflows/`); `rtk proxy grep -rln "console" docs/pillars/` returns the new file.

### Step 5: Format, lint, tests, docs lint

`cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --lib`, plus the docs lint for step 4.

## Test plan

Store + handler tests per steps 1–2. The bind warning and docs are verified by predicate test / doc lint. No test writes to the real `sessions.db` (use the in-memory store for store tests; `ENV_LOCK`+`HomeGuard` for handler tests).

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib` exits 0 with the fork tests
- [ ] `rtk proxy grep -n "sessions/{id}/fork\|/fork" src/gateway/api_v1.rs` returns a match
- [ ] `rtk proxy grep -n "fork_session" src/sessions/store.rs` returns a match; `split_session` is NOT deleted
- [ ] `rtk proxy grep -n "backfill_titles" src/gateway/mod.rs` returns a match
- [ ] `rtk proxy grep -rn "no login configured" src/gateway/mod.rs` returns a match
- [ ] `docs/pillars/10-web-console.md` exists and is linked from `docs/SUMMARY.md`
- [ ] the contract ADR exists
- [ ] No files outside the in-scope list modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

- `split_session` was deleted by plan 221 (it should not have been) — restore it or build `fork_session` fresh from its body; report the plan-221 drift.
- Fork semantics beyond the minimal v1 (whole-parent replay, summary generation via the model) are requested — that is a larger feature; ship the minimal non-destructive fork and note the richer version as a follow-up.
- The docs repo has a specific ADR location/format convention that differs from the guess — follow the existing convention (`rtk proxy grep -rln adr docs/`); do not invent a new tree.
- A step's verification fails twice after a reasonable fix.

## Maintenance notes

- The claw-ui side (a "Fork" button on a session, a "Download transcript" action from `sessions_get`, and an Insights tile row from `/api/v1/insights`) is **plan 232** — it depends on this plan's fork route.
- If model-generated fork summaries are added later, they belong behind the same route with a `summary_mode` field, not a new route.
- Reviewer focus: `fork_session` leaves the parent open (unlike `split_session`); the bind warning fires only for exposed+login-off; the pillar doc claims match what ships.
