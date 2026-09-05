# Plan 039: Wire the web console's skills-management panel to real gateway routes (list/install/enable/disable/uninstall end to end)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> This plan spans **two repositories**:
> - Rust gateway: `/home/sulthannauval/project/rantai/RantAIClaw`
> - Next.js console (`claw-ui`): `/home/sulthannauval/project/rantai/claw-ui`
>
> **Drift check (run first)**, from the RantAIClaw repo root:
> ```
> git diff --stat 4736e2e..HEAD -- src/gateway/api_v1.rs src/gateway/mod.rs src/skills/mod.rs src/skills/clawhub.rs src/config/schema.rs docs/reference/api-v1.md docs/reference/config.md
> ```
> and from the claw-ui repo root:
> ```
> git diff --stat -- src/components/ops/skills-panel.tsx src/lib/api.ts src/lib/types.ts src/app/api/rc/'[...path]'/route.ts
> ```
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED-HIGH (adds mutating routes on an exposure surface — see the exposure-boundary section)
- **Depends on**: `plans/037-*.md` (skills enable/disable config writer) and `plans/034-*.md` (true skill uninstall). Both are **hard** dependencies for Steps 3 and 4 respectively. `plans/035-*.md` (skill update) is intentionally **out of scope** here (noted as follow-up). NOTE: as of the planning commit, plans 034/035/037 files did **not** yet exist in `plans/`; if the backend functions they introduce are absent when you reach Steps 3–4, that is a STOP condition (see STOP conditions).
- **Category**: bug (dead UI) + direction (web management parity)
- **Planned at**: commit `4736e2e`, 2026-07-23

## Why this matters

`claw-ui` ships a **fully-built** skills-management panel — a Power enable/disable
toggle, an Install button, and an Uninstall-with-confirm dialog — but the Rust
gateway registers **only** `GET /api/v1/skills` and `GET /api/v1/skills/{name}`.
Every mutating button in the console therefore hits a route the router does not
have: `POST /api/v1/skills/install` and `PUT /api/v1/skills/{name}/enabled` land
on the GET-only skills paths (405 / 404) and `DELETE /api/v1/skills/{name}` the
same, so each click ends in an error toast. On top of that the read endpoint
never sends the `enabled`/`reasons` fields the UI renders, so the toggle always
shows "on" and skills disabled in config silently vanish from the list. This plan
makes the console's skills panel work end to end: list with correct
enabled/disabled/gated state, install, enable, disable, and uninstall — by adding
gateway routes that **reuse existing backend implementations**, not by
duplicating logic.

## Current state

### Rust gateway (RantAIClaw)

- `src/gateway/api_v1.rs` — the `/api/v1/*` control-plane router and handlers.
  - Router (only skills routes shown), `src/gateway/api_v1.rs:37-68`:
    ```rust
    pub fn router() -> Router<AppState> {
        Router::new()
            .route("/api/v1/version", get(version))
            // …
            .route("/api/v1/skills", get(skills_list))
            .route("/api/v1/skills/{name}", get(skills_show))
            .route("/api/v1/memory", get(memory_list))
            // …
    }
    ```
    `use axum::routing::{get, post, put};` is already imported at
    `api_v1.rs:27` — add `delete` to that import.
  - `skills_list`, `src/gateway/api_v1.rs:943-965` — **the read-contract bug**:
    ```rust
    async fn skills_list(
        State(state): State<AppState>,
        headers: HeaderMap,
    ) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
        check_auth(&state, &headers)?;
        let cfg = state.config.lock().clone();
        let skills = crate::skills::load_skills_with_config(&cfg.workspace_dir, &cfg);
        let json: Vec<_> = skills
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "version": s.version,
                    "description": s.description,
                    "tags": s.tags,
                    "tools": s.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                })
            })
            .collect();
        Ok(Json(
            serde_json::json!({ "skills": json, "count": json.len() }),
        ))
    }
    ```
    `load_skills_with_config` **filters out** disabled and requires-gated skills
    (see below), so they never reach the console, and no `enabled`/`reasons` is
    ever emitted.
  - `skills_show`, `src/gateway/api_v1.rs:967-989` — same `load_skills_with_config`
    source; must stay consistent with the new list fields.
  - Error helpers, `src/gateway/api_v1.rs:128-162`: `struct ErrorBody { error, detail }`,
    `err_500(anyhow::Error)` → 500, `err_404(msg)` → 404, `err_400(msg)` → 400.
    Handlers return `Result<Json<Value>, (StatusCode, Json<ErrorBody>)>`.
  - `check_auth(&state, &headers)?`, `src/gateway/api_v1.rs:102-125` — bearer/pairing
    gate. Returns `Ok(())` immediately when `require_pairing == false`; otherwise
    requires `Authorization: Bearer <token>` matching a paired token. Every
    sibling handler calls it first. **All new handlers must call it first too.**

- `src/skills/mod.rs` — skill loading.
  - `load_skills_with_config(workspace_dir, config) -> Vec<Skill>`, `src/skills/mod.rs:225-251`
    — drops skills whose `[skills.entries.<name>] enabled = false` and skills with
    unmet `requires`. This is why disabled skills disappear from the admin list.
  - `load_skills_with_status(workspace_dir, config) -> Vec<(Skill, Vec<String>)>`,
    `src/skills/mod.rs:256-279` — **already computes** the data the UI needs:
    ```rust
    pub fn load_skills_with_status(
        workspace_dir: &Path,
        config: &crate::config::Config,
    ) -> Vec<(Skill, Vec<String>)> {
        let raw = load_skills_with_open_skills_config(/* … */);
        let mut out: Vec<(Skill, Vec<String>)> = raw
            .into_iter()
            .map(|s| {
                let mut reasons = s.requires.unmet();
                if let Some(entry) = config.skills.entries.get(&s.name) {
                    if !entry.enabled {
                        reasons.insert(0, "disabled in config.toml".to_string());
                    }
                }
                (s, reasons)
            })
            .collect();
        out.sort_by_key(|(_, reasons)| !reasons.is_empty()); // active first
        out
    }
    ```
    `reasons.is_empty()` ⇒ the skill is fully active (enabled AND requirements met).
    The `"disabled in config.toml"` reason is inserted **first** when the config
    flag is off.

- `src/skills/clawhub.rs` — ClawHub install backend.
  - `pub async fn install_one(profile: &Profile, slug: &str) -> Result<()>`,
    `src/skills/clawhub.rs:365-387` — validates the slug, is **idempotent**
    (returns `Ok(())` without re-installing if `profile.skills_dir().join(slug)`
    already exists), and cleans up a partial dir on failure. It calls
    `validate_slug` internally as its first line.
  - `fn validate_slug(slug: &str) -> Result<()>`, `src/skills/clawhub.rs:553-567` —
    **currently private** (`fn`, not `pub`). Rejects empty, `/`, `\`, `..`, and any
    char outside `[a-z0-9-_]`. To pre-validate an install/uninstall request and
    return a clean `400` (instead of a `500` bubbling out of `install_one`), you
    will make this `pub(crate)` in Step 3.
  - Profile resolution used by every install caller (e.g. `src/skills/mod.rs:1302,1465`,
    `src/tools/skills_install.rs`): `crate::profile::ProfileManager::active()`
    returns `anyhow::Result<Profile>`. `install_one` takes `&Profile`.

- **Config-write plumbing lives in a different file.** `src/gateway/config_api.rs`
  holds the read-modify-write helpers `lock_and_load()` (`config_api.rs:232-239`)
  and `persist_and_swap(&state, cfg)` (`config_api.rs:244-248`), plus the
  `CONFIG_WRITE_LOCK`. These are **private to `config_api.rs`**. The exemplar
  owner-scoped mutation there is `remove_mcp_server` (`config_api.rs:439-452`):
  ```rust
  async fn remove_mcp_server(
      State(state): State<AppState>, headers: HeaderMap, Path(name): Path<String>,
  ) -> Result<Json<serde_json::Value>, ApiError> {
      check_auth(&state, &headers)?;
      let (_guard, mut cfg) = lock_and_load().await?;
      let removed = cfg.mcp_servers.remove(&name).is_some();
      let count = cfg.mcp_servers.len();
      persist_and_swap(&state, cfg).await?;
      Ok(Json(json!({ "name": name, "removed": removed, "count": count })))
  }
  ```
  **Implication for Step 3**: the `PUT enabled` handler must NOT re-implement the
  config read-modify-write. It must call the standalone enable/disable **writer
  function introduced by plan 037** (which encapsulates
  `skills.entries.<name>.enabled` persistence). If that function does not exist
  when you get there, STOP — do not copy `lock_and_load`/`persist_and_swap` into
  `api_v1.rs` (that would duplicate config-write policy across two modules and
  break plan 037's ownership of the writer).

- `src/config/schema.rs:549-566` — `struct SkillEntryConfig { enabled: bool (default true), api_key, env, config }`. The `enabled` field the writer flips **already exists**; this plan adds no new config key.
- `src/config/migrations.rs:36` — `pub const CURRENT_VERSION: u32 = 15;` (schema version; relevant only if a gating flag is later added — see exposure section).
- `docs/reference/api-v1.md:380-419` — the `## Skills` section documenting the two GET endpoints (response shapes shown). New endpoints get documented here.
- `docs/reference/config.md:131-170` — the `## [skills]` section documenting `[skills.entries.<name>] enabled`.

### Next.js console (claw-ui) — already matches the target routes

- `src/lib/api.ts:171-200` — the client already calls exactly the routes this plan adds:
  ```ts
  setSkillEnabled: (name: string, enabled: boolean) =>
    rc<{ name: string; enabled: boolean }>(
      `skills/${encodeURIComponent(name)}/enabled`,
      { method: "PUT", body: JSON.stringify({ enabled }) },
    ),
  installSkill: (slug: string) =>
    rc<{ slug: string; installed: boolean }>("skills/install", {
      method: "POST", body: JSON.stringify({ slug }),
    }),
  uninstallSkill: (name: string) =>
    rc<{ name: string; removed: boolean }>(
      `skills/${encodeURIComponent(name)}`, { method: "DELETE" },
    ),
  ```
  and the list reader `skills: () => rc<{ skills: Skill[]; count: number }>("skills")` (`api.ts:74`).
- `src/lib/types.ts:62-71` — `interface Skill` **already declares** the optional fields the read fix will populate:
  ```ts
  export interface Skill {
    name: string; version: string | null; description: string | null;
    tags: string[]; tools: string[];
    enabled?: boolean; active?: boolean; reasons?: string[];
  }
  ```
- `src/components/ops/skills-panel.tsx` — the panel. Handlers (`skills-panel.tsx:63-101`):
  ```ts
  const toggle = async (name, enabled) => { await api.setSkillEnabled(name, enabled); /* toast + refresh */ };
  const install = async (slug) => { await api.installSkill(slug); /* toast + refresh */ };
  const uninstall = async () => { await api.uninstallSkill(pendingUninstall); /* toast + refresh */ };
  ```
  Render reads `const enabled = s.enabled !== false;` (`skills-panel.tsx:129`) and
  shows a `disabled` badge + dimmed card when `enabled === false`
  (`skills-panel.tsx:132,136`).
- `src/app/api/rc/[...path]/route.ts` — a **blind, method-preserving proxy**: it
  relays GET/POST/PUT/DELETE verbatim (with the server-side bearer token) to
  `<gateway>/api/v1/<path>`, confined to `/api/v1/*`. **No claw-ui code change is
  required for routing.** The only claw-ui work is verification (Step 6).

### Response shapes the console expects (must match exactly)

| UI call | Route | Request body | Success response |
|---|---|---|---|
| `installSkill(slug)` | `POST /api/v1/skills/install` | `{ "slug": "<slug>" }` | `{ "slug": "<slug>", "installed": true }` |
| `setSkillEnabled(name, enabled)` | `PUT /api/v1/skills/{name}/enabled` | `{ "enabled": <bool> }` | `{ "name": "<name>", "enabled": <bool> }` |
| `uninstallSkill(name)` | `DELETE /api/v1/skills/{name}` | none | `{ "name": "<name>", "removed": true }` |
| `skills()` list | `GET /api/v1/skills` | none | `{ "skills": [{…, "enabled": bool, "active": bool, "reasons": [..] }], "count": N }` |

## Exposure-boundary justification (CLAUDE.md §3.6 / §10)

Adding mutating skill routes **widens an exposure surface**: skill install fetches
and stages community code onto the operator's machine, and it becomes reachable
over the pairing-authenticated HTTP API rather than only the local CLI/TUI. Per
CLAUDE.md §3.6, exposure surfaces are deny-by-default and any widening needs
**explicit justification**. This section is the justification; the reviewer must
sign off on it.

**Justification (why this is acceptable and in-class with existing routes):**

1. **Owner-scoped, never anonymous.** Every new handler MUST call
   `check_auth(&state, &headers)?` as its first line, exactly like `skills_list`
   (`api_v1.rs:947`) and `remove_mcp_server` (`config_api.rs:444`). With
   `gateway.require_pairing = true` (the default), only a caller holding a paired
   bearer token — i.e. the operator/owner — can reach these routes. The only
   anonymous path is the pre-existing, documented `require_pairing = false`
   escape hatch (`api_v1.rs:102-105`), which already opens every route in this
   file; this plan adds no new anonymous surface.
2. **In-class with precedent already shipped on this same API.** `config_api.rs`
   already lets the pairing-authed owner **add MCP servers** (arbitrary command
   configuration → code execution, `add_mcp_server`/`remove_mcp_server`), **set
   autonomy**, and **connect a Telegram bot** over this exact auth gate. Skill
   install (staging community code) is the same risk class and the same single
   authenticated principal. No *new* class of exposure is opened; this is parity
   with mutation routes already accepted on this surface.
3. **Capability parity, not escalation.** The authenticated owner can already
   install/uninstall/toggle skills via the local CLI (`skills install/remove`)
   and TUI. These routes give the *same principal* the *same capability* through
   the console they are already authenticated to.

**Schema / schema-drift note (be precise — do not over-claim a bump):** the
schema-drift gate fingerprints config **defaults**. This plan introduces **no new
config key and changes no default** — it reuses the existing
`SkillEntryConfig.enabled` field (`schema.rs:551`, default already `true`) and the
existing install/uninstall backends. Therefore the config-default fingerprint does
not change and **no `schema_version` bump is forced by this plan**. Record the
exposure widening in the PR description / CHANGELOG per §3.6 and §10 regardless
(it is a behavior/exposure change even without a schema change). **If** the
reviewer chooses the deny-by-default hardening below (a new gating flag), that
*does* add a new default → bump `CURRENT_VERSION` `15 → 16` in
`src/config/migrations.rs:36` and refresh the schema-drift snapshot; that is the
only path in this plan that touches the schema version.

**Threat note for the PR:** compromise of the pairing token now additionally
grants remote skill install (community-code staging) and enable/disable/uninstall.
Mitigations already in place and to be preserved: `install_one` validates the slug
and is all-or-nothing with partial-dir cleanup (`clawhub.rs:365-387`); the DELETE
identifier must be traversal-guarded (Step 4 reuses the same slug/remove guards).

**Optional deny-by-default hardening (reviewer's call — NOT required by this plan):**
gate the three mutating routes behind a new `gateway.allow_skill_management` config
flag. If chosen, default it to `true` to preserve the user's out-of-the-box
web-management goal (documented widening of a *local-management* convenience, not a
network-reach default), OR `false` for strict deny-by-default at the cost of
first-run friction. Either way it is a new config default → schema bump + drift
snapshot as noted above. Presented as a follow-up option; do **not** implement it
in this plan unless the reviewer explicitly asks.

## Alternative (cheaper, LOW-risk) — documented, NOT recommended

Instead of adding backend routes, **hide the dead controls** so the console never
shows buttons that 404/405. In `claw-ui/src/components/ops/skills-panel.tsx`, gate
the Install button (`skills-panel.tsx:213-223`), the Power toggle
(`skills-panel.tsx:138-149`), and the Uninstall control
(`skills-panel.tsx:150-158` + the `ConfirmModal` at `:249-261`) behind a
build-time feature flag (e.g. `process.env.NEXT_PUBLIC_SKILLS_MGMT === "1"`,
default off). The panel degrades to read-only listing until the backend exists.

- **Pros**: zero backend/exposure change (LOW risk), removes the broken UX today,
  fully reversible by flipping the flag.
- **Cons**: does **not** deliver web skill management — the user's explicit goal.
  Read-only listing still needs the Step 2 read-contract fix to show
  enabled/disabled state correctly.

**Recommendation: implement the routes (Steps 1–6).** The user asked for web
management to work end to end; the alternative is the fallback only if the
`plans/037`/`plans/034` backend dependencies cannot be satisfied.

## Commands you will need

RantAIClaw (run from `/home/sulthannauval/project/rantai/RantAIClaw`):

| Purpose | Command | Expected on success |
|---|---|---|
| Format check | `cargo fmt --all -- --check` | exit 0, no diff |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0, no warnings |
| Targeted tests | `cargo test --lib gateway::api_v1` | all pass, incl. new tests |
| (fallback filter) | `cargo test --lib skills` | all pass |

claw-ui (run from `/home/sulthannauval/project/rantai/claw-ui`):

| Purpose | Command | Expected on success |
|---|---|---|
| Build | `pnpm build` | exit 0 |
| Lint | `pnpm lint` | exit 0 |

## Suggested executor toolkit

- Invoke the `rust-skills` skill when writing the new handlers (error handling,
  async, API design conventions).
- Reference `docs/reference/api-v1.md` (existing endpoint doc style) before
  writing the doc updates in Step 5.

## Scope

**In scope — RantAIClaw** (the only source files you should modify):
- `src/gateway/api_v1.rs` — router additions, `skills_list`/`skills_show` field
  additions, three new handlers, new tests.
- `src/skills/clawhub.rs` — change `validate_slug` from `fn` to `pub(crate) fn`
  (single-token visibility change; do not alter its logic).
- `docs/reference/api-v1.md` — document the three new endpoints + the new list fields.
- `docs/reference/config.md` — one note that `[skills.entries.<name>] enabled` is
  now togglable via the API (no schema change).

**In scope — claw-ui**: **verification only** (Step 6). Only touch source if Step 6
surfaces a concrete shape mismatch (none expected).

**Out of scope — do NOT touch:**
- `src/gateway/config_api.rs` — do not move/duplicate its `lock_and_load` /
  `persist_and_swap` helpers into `api_v1.rs`. The enable/disable writer belongs to
  plan 037.
- The skill **update** route (`plans/035`) — explicitly deferred; do not add it.
- `src/gateway/mod.rs` merge site (`mod.rs:779-805`) — `api_v1::router()` is
  already merged with its rate-limit + body-limit layers; adding routes inside
  `router()` needs no change here. Do not touch the merge.
- Any change to the public response shape of the two existing GET endpoints beyond
  the *additive* `enabled`/`active`/`reasons` fields.
- The `name`-vs-`slug` identity reconciliation (see Maintenance notes) — coordinate
  with plan 034, do not redesign it here.

## Git workflow

- Branch: `advisor/039-gateway-skills-management-api`.
- Conventional commits, matching repo history (e.g.
  `feat(gateway): skills management API — install/enable/disable/uninstall routes`).
  Commit per logical unit (read-fix, then each route, then docs, then tests) so
  rollback stays granular.
- **Do NOT add a `Co-Authored-By` trailer** (repo convention).
- Do NOT push or open a PR unless the operator instructed it.
- Executor updates the `plans/README.md` status row for 039 when done.

## Steps

Order matters: do the additive read fix first (safe, unblocks the UI display),
then add mutating routes one at a time so the build stays green between commits.

### Step 1: Confirm the two backend dependencies exist; STOP if not

Before writing any mutating handler, verify the functions Steps 3 and 4 call:

- **Plan 037 writer** — a standalone function that sets
  `skills.entries.<name>.enabled` and persists it. Search:
  ```
  grep -rn "fn set_skill_enabled\|skills.entries\|entries.*enabled\|fn set_enabled" src/skills/ src/config/
  ```
  Expected: a `pub` function (likely `crate::skills::set_skill_enabled(name, enabled)`
  or similar in `src/skills/mod.rs` or `src/config/`) that does the read-modify-write
  atomically. Note its exact path + signature.
- **Plan 034 uninstall** — a function that removes an installed skill's directory
  (and clears its config entry). Search:
  ```
  grep -rn "fn uninstall_one\|fn remove_skill\|fn uninstall_skill\|pub async fn uninstall" src/skills/
  ```
  Expected: a `pub` function (likely `crate::skills::clawhub::uninstall_one(&profile, name)`
  or `crate::skills::remove_skill(...)`). Note its exact path, signature, and what
  identifier it takes (skill **name** vs directory **slug** — this determines Step 4's
  path-param handling).

**STOP** and report if either function is missing: plans 034/037 have not landed
yet and this plan cannot reuse a non-existent backend. Do not build the mutating
route against a hand-rolled substitute — that would duplicate config-write/uninstall
policy this plan is explicitly forbidden from owning.

**Verify**: both greps return a concrete `pub`/`pub(crate)` function definition.

### Step 2: Fix the read contract — `skills_list` and `skills_show` emit `enabled`/`active`/`reasons`

In `src/gateway/api_v1.rs`, rewrite `skills_list` (`:943-965`) to source from
`load_skills_with_status` and stop hiding disabled/gated skills:

```rust
async fn skills_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let cfg = state.config.lock().clone();
    let skills = crate::skills::load_skills_with_status(&cfg.workspace_dir, &cfg);
    let json: Vec<_> = skills
        .iter()
        .map(|(s, reasons)| {
            // `enabled` reflects ONLY the config flag (what the UI toggle drives);
            // `active` = fully loaded (enabled AND requirements met);
            // `reasons` = why it's not active (first entry is "disabled in config.toml"
            // when the flag is off, per load_skills_with_status).
            let enabled = cfg
                .skills
                .entries
                .get(&s.name)
                .map(|e| e.enabled)
                .unwrap_or(true);
            serde_json::json!({
                "name": s.name,
                "version": s.version,
                "description": s.description,
                "tags": s.tags,
                "tools": s.tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                "enabled": enabled,
                "active": reasons.is_empty(),
                "reasons": reasons,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "skills": json, "count": json.len() })))
}
```

Update `skills_show` (`:967-989`) consistently: switch its source to
`load_skills_with_status`, find the `(skill, reasons)` tuple by case-insensitive
name (preserve the existing `err_404` on miss), and add the same
`enabled`/`active`/`reasons` fields to its JSON (keep the richer per-tool
`{name, description}` shape it already returns).

**Verify**:
- `cargo fmt --all -- --check` → exit 0
- `cargo clippy --all-targets -- -D warnings` → exit 0
- `cargo test --lib gateway::api_v1` → all pass (existing tests still green)

### Step 3: Add `PUT /api/v1/skills/{name}/enabled`

Add `delete` and confirm `put` in the `use axum::routing::{…}` import
(`api_v1.rs:27`). In `router()` (`:53-54`), extend the skills routes:

```rust
.route("/api/v1/skills", get(skills_list))
.route("/api/v1/skills/install", post(skills_install))
.route(
    "/api/v1/skills/{name}",
    get(skills_show).delete(skills_uninstall),
)
.route("/api/v1/skills/{name}/enabled", put(skills_set_enabled))
```
(Register `/api/v1/skills/install` **before** `/api/v1/skills/{name}` is not
required by axum's matcher — a static segment wins over a capture — but keeping the
static route listed first aids readability.)

Add the handler. It calls the **plan-037 writer** found in Step 1 (adjust the
function path/name to what you found):

```rust
#[derive(serde::Deserialize)]
struct SkillEnabledBody {
    enabled: bool,
}

async fn skills_set_enabled(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<SkillEnabledBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    crate::skills::clawhub::validate_slug(&name).map_err(|e| err_400(format!("{e:#}")))?;
    // Plan-037 writer: flips [skills.entries.<name>] enabled and persists + swaps
    // the running config. Use the exact function path found in Step 1.
    crate::skills::set_skill_enabled(&state, &name, body.enabled)
        .await
        .map_err(err_500)?;
    Ok(Json(serde_json::json!({ "name": name, "enabled": body.enabled })))
}
```

Notes:
- If plan 037's writer signature differs (e.g. it does not take `&state` and you
  must swap the running config separately), follow the writer's own contract — do
  **not** re-implement `lock_and_load`/`persist_and_swap` in this file.
- `validate_slug` is reused as the `{name}` guard (traversal/charset). Making it
  `pub(crate)` happens in Step 4's edit; if you land Step 3 first, do the
  visibility change now.

**Verify**: `cargo test --lib gateway::api_v1` → pass; `cargo clippy --all-targets -- -D warnings` → exit 0.

### Step 4: Add `POST /api/v1/skills/install` and `DELETE /api/v1/skills/{name}`

First make the slug guard reusable — in `src/skills/clawhub.rs:553` change:
```rust
fn validate_slug(slug: &str) -> Result<()> {
```
to:
```rust
pub(crate) fn validate_slug(slug: &str) -> Result<()> {
```
(logic unchanged; this is the only edit to `clawhub.rs`).

Add the install handler to `api_v1.rs`:

```rust
#[derive(serde::Deserialize)]
struct SkillInstallBody {
    slug: String,
}

async fn skills_install(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SkillInstallBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    let slug = body.slug.trim().to_string();
    crate::skills::clawhub::validate_slug(&slug).map_err(|e| err_400(format!("{e:#}")))?;
    let profile = crate::profile::ProfileManager::active().map_err(err_500)?;
    crate::skills::clawhub::install_one(&profile, &slug)
        .await
        .map_err(err_500)?;
    Ok(Json(serde_json::json!({ "slug": slug, "installed": true })))
}
```
(`install_one` is idempotent — a slug already present returns `Ok(())`; reporting
`"installed": true` for an already-present skill is correct: it is installed.)

Add the uninstall handler, calling the **plan-034 uninstall** found in Step 1
(adjust name/signature to what you found — this example assumes
`crate::skills::clawhub::uninstall_one(&profile, name) -> Result<bool>` where the
bool is "was present"):

```rust
async fn skills_uninstall(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorBody>)> {
    check_auth(&state, &headers)?;
    crate::skills::clawhub::validate_slug(&name).map_err(|e| err_400(format!("{e:#}")))?;
    let profile = crate::profile::ProfileManager::active().map_err(err_500)?;
    let removed = crate::skills::clawhub::uninstall_one(&profile, &name)
        .await
        .map_err(err_500)?;
    Ok(Json(serde_json::json!({ "name": name, "removed": removed })))
}
```
If plan 034's uninstall returns `Result<()>` (no bool) and 404s on unknown itself,
map that error to `err_404` and return `"removed": true` on `Ok`. Match its actual
contract; the console only reads `removed` as a boolean.

**Coordination point (do not redesign, just honor):** the DELETE path param and the
identifier plan 034's uninstall expects must be the same thing the list endpoint
returns. The console calls `uninstallSkill(s.name)` using the skill **name** from
the list. If plan 034 removes by directory **slug** and name ≠ slug for some
skills, uninstall of those skills will miss. If Step 1 shows this divergence, record
it as a STOP/report item rather than papering over it.

**Verify**: `cargo fmt --all -- --check` → exit 0; `cargo clippy --all-targets -- -D warnings` → exit 0; `cargo test --lib gateway::api_v1` → pass.

### Step 5: Document the new endpoints and config behavior

- `docs/reference/api-v1.md` — in the `## Skills` section (`:380-419`):
  - Update the `GET /api/v1/skills` response block to include
    `"enabled": true, "active": true, "reasons": []` and one line explaining
    `active`/`reasons` (disabled or requires-gated skills now appear, flagged).
  - Add `POST /api/v1/skills/install` (body `{ "slug": "<slug>" }`, response
    `{ "slug", "installed" }`, status `200/400/401/500`), `PUT
    /api/v1/skills/{name}/enabled` (body `{ "enabled": bool }`, response
    `{ "name", "enabled" }`, status `200/400/401/500`), and `DELETE
    /api/v1/skills/{name}` (response `{ "name", "removed" }`, status
    `200/400/401/404/500`). State each is **bearer-gated (owner/pairing)**.
  - Add a one-line security note mirroring the exposure justification: these are
    owner-scoped mutations equivalent to the local CLI, install stages community code.
- `docs/reference/config.md` — in `## [skills]` (`:131-170`), add one sentence that
  `[skills.entries.<name>] enabled` is now toggleable at runtime via
  `PUT /api/v1/skills/{name}/enabled` (no schema change; same key).

**Verify**: no command required, but re-read both edits for accuracy against the
handlers you wrote.

### Step 6: Verify claw-ui end to end (no code change expected)

- Confirm the client already matches (it does — `api.ts:171-200`, `types.ts:62-71`,
  proxy `route.ts` relays verbatim). Do **not** edit unless a mismatch is found.
- Build + lint from `/home/sulthannauval/project/rantai/claw-ui`:
  - `pnpm build` → exit 0
  - `pnpm lint` → exit 0
- claw-ui has **no E2E harness**. If a running gateway + console pair is available,
  drive the panel manually (or with Playwright): install a skill from Browse
  ClawHub, toggle it off (badge shows `disabled`, card dims), toggle on, uninstall
  via the confirm dialog — each should toast success and refresh. If no live rig is
  available, `pnpm build` passing + the gateway route tests (below) are the accepted
  evidence; note in the handoff that live drive was not run.

## Test plan

Add tests to the existing `#[cfg(test)] mod tests` in `src/gateway/api_v1.rs`
(`:1311+`). Handlers are invoked **directly** (not through a live server) with
`State(test_state())`, `HeaderMap`, `Path(..)`, `Json(..)` — model after
`resolve_approval_endpoint_resolves_pending_request` (`:1651`) and
`resolve_approval_endpoint_unknown_id_is_404` (`:1679`). `test_state()`
(`:1420`) sets `require_pairing = false`; for auth-required assertions, build a
state with pairing on (see below).

Cases to cover:
- **Read fix**: `skills_list` on a config with `[skills.entries.<x>] enabled = false`
  returns that skill with `"enabled": false` and a non-empty `"reasons"` containing
  `"disabled in config.toml"`, and it is NOT filtered out (present in `skills`).
  A fully-active skill returns `"active": true, "reasons": []`. (Use a temp
  workspace with a minimal SKILL.md, or assert against whatever fixture skills the
  existing skills tests use — check `src/skills/mod.rs` tests for the fixture
  pattern.)
- **Auth required**: call `skills_install` / `skills_set_enabled` / `skills_uninstall`
  with a `require_pairing = true` state and an empty `HeaderMap` → each returns
  `Err` with `StatusCode::UNAUTHORIZED`. (Construct the state like `test_state()`
  but with `require_pairing = true` and `PairingGuard::new(true, &[])`.)
- **Slug validation reject**: `skills_install` with `slug = "../evil"` (or
  `"a/b"`) → `Err(StatusCode::BAD_REQUEST)`; same for `skills_set_enabled` /
  `skills_uninstall` with a traversal `{name}`.
- **Install happy path**: requires network/ClawHub — gate behind the same
  mechanism existing clawhub tests use (they likely don't hit the network in
  `--lib`). Prefer testing the **validation + profile-resolution + response-shape**
  path and asserting the JSON envelope keys (`slug`, `installed`) rather than a real
  download. If a hermetic install fixture is not available, assert the 400/401
  paths and the response-envelope construction only, and note the live install is
  covered by Step 6's manual drive.
- **Enable/disable happy path**: `skills_set_enabled(name, false)` then a
  `skills_list` shows `"enabled": false`; toggling back to `true` shows
  `"enabled": true`. (Depends on plan 037's writer being callable against the test
  config — if the writer requires an on-disk profile, use the `HomeGuard` +
  `ENV_LOCK` pattern already in this test module, `:1401`, `:1544`.)
- **Uninstall unknown skill**: `skills_uninstall` with a `{name}` that isn't
  installed → `Ok` with `"removed": false` (or `404` if plan 034's function 404s —
  match its contract; assert whichever it is).

**Verification**: `cargo test --lib gateway::api_v1` → all pass, including the new
tests. `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`
→ exit 0.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0 (RantAIClaw)
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0 (RantAIClaw)
- [ ] `cargo test --lib gateway::api_v1` exits 0; new tests (auth-required,
      slug-reject, read-fix enabled/reasons, enable/disable, uninstall-unknown) exist and pass
- [ ] `router()` registers `POST /api/v1/skills/install`,
      `PUT /api/v1/skills/{name}/enabled`, and `.delete(skills_uninstall)` on
      `/api/v1/skills/{name}` (`grep -n "skills/install\|skills/{name}/enabled\|skills_uninstall" src/gateway/api_v1.rs`)
- [ ] `skills_list` sources from `load_skills_with_status` and emits
      `enabled`/`active`/`reasons` (`grep -n "load_skills_with_status" src/gateway/api_v1.rs` returns the list+show handlers)
- [ ] No `lock_and_load`/`persist_and_swap` copied into `api_v1.rs`
      (`grep -n "lock_and_load\|persist_and_swap" src/gateway/api_v1.rs` returns nothing)
- [ ] `pnpm build` and `pnpm lint` exit 0 (claw-ui); no claw-ui source changed
      unless a mismatch was found and reported
- [ ] `docs/reference/api-v1.md` documents the three new endpoints + the new list fields
- [ ] No files outside the in-scope list are modified (`git status` in both repos)
- [ ] `plans/README.md` status row for 039 updated

## STOP conditions

Stop and report back (do not improvise) if:

- The plan-037 enable/disable writer or the plan-034 uninstall function does not
  exist (Step 1). Do NOT hand-roll a config read-modify-write or a directory
  removal in `api_v1.rs` — those belong to 037/034. This plan is BLOCKED on them.
- The code at the "Current state" locations doesn't match the excerpts (drift since
  commit `4736e2e`).
- The plan-034 uninstall removes by an identifier (slug/dir) that differs from the
  skill **name** the list endpoint returns, such that the console's
  `uninstallSkill(s.name)` cannot reliably target it. Report the divergence; do not
  invent a name↔slug mapping here.
- A step's verification fails twice after a reasonable fix attempt.
- The fix appears to require touching an out-of-scope file (especially
  `config_api.rs` or `mod.rs`'s merge site).
- The reviewer requires deny-by-default gating via a new config flag — that adds a
  config default and forces a `schema_version` bump + drift-snapshot refresh, which
  is out of this plan's stated scope; get explicit sign-off before doing it.

## Maintenance notes

For the human/agent who owns this code after the change lands:

- **name vs slug**: the console lists by skill **name** (SKILL.md frontmatter) but
  installs by ClawHub **slug** (`install_one`'s directory key). The Browse-tab
  "installed" check (`skills-panel.tsx:198`) already compares `slug` against
  installed **names**, a pre-existing latent mismatch. If a future skill's name ≠
  slug, both that check and DELETE-by-name can misfire. A follow-up should return
  the install slug/dir in the list payload and have the console target uninstall by
  it. Out of scope here.
- **Skill update route** (`plans/035`, `PUT`/`POST .../update` or re-install) is a
  deliberate follow-up. When added, it slots into the same `router()` block and
  reuses the same auth + slug guard; keep its response shape aligned with the
  console client if/when the UI grows an "update" affordance.
- **Reviewer should scrutinize**: (1) every new handler calls `check_auth` first;
  (2) no config-write plumbing was duplicated out of `config_api.rs`; (3) slug/name
  is `validate_slug`-guarded on all three mutating routes; (4) the exposure widening
  is called out in the PR body per §3.6/§10; (5) the additive-only change to the
  two GET response shapes (no removed/renamed fields).
- **If a `gateway.allow_skill_management` flag is added later** (the deny-by-default
  hardening), remember it is a new config default → `CURRENT_VERSION` bump in
  `src/config/migrations.rs` + schema-drift snapshot, and the console needs a
  read of it (e.g. via `auth/info` or `config`) to hide the controls when off.
