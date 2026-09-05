# Plan 019: Document the `/api/v1` HTTP surface as a versioned reference (optionally test-enforced)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/gateway/api_v1.rs`
> If it changed since this plan was written, re-read the router and reconcile the
> route list before documenting; on a large structural mismatch, treat it as a
> STOP condition.

## Status

- **Priority**: P3
- **Effort**: M
- **Risk**: LOW
- **Depends on**: plan 013 (only for the optional test-enforcement in Step 3; the
  docs themselves have no dependency)
- **Category**: docs / direction
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

There is a rich, already-built, auth-gated HTTP API (`/api/v1`, 22 routes:
sessions, memory, skills, insights, providers, agent/chat, approvals, …) that the
README's promised "digital employee platform / dashboard" necessarily consumes.
But the only reference doc is a streaming-only note; the full contract is
undocumented and unversioned. That means only the bundled console can safely
target it. Documenting it (a reference page, and ideally an OpenAPI spec) is the
cheapest path to third-party dashboards/integrations and stabilizes the surface
the README already promises a platform around. Pair with plan 013 so the
documented contract is test-enforced rather than aspirational.

## Current state

- `src/gateway/api_v1.rs:30-63` — the router with all 22 routes (verified):
  ```rust
  pub fn router() -> Router<AppState> {
      Router::new()
          .route("/api/v1/version", get(version))
          .route("/api/v1/auth/info", get(auth_info))
          .route("/api/v1/status", get(status))
          .route("/api/v1/doctor", get(doctor))
          .route("/api/v1/agent/chat", post(agent_chat))
          .route("/api/v1/approvals/{id}", post(resolve_approval))
          .route("/api/v1/sessions", get(sessions_list))
          .route("/api/v1/sessions/search", post(sessions_search))
          .route("/api/v1/sessions/{id}", get(sessions_get).delete(sessions_delete))
          .route("/api/v1/sessions/{id}/title", put(sessions_set_title))
          .route("/api/v1/insights", get(insights))
          .route("/api/v1/skills", get(skills_list))
          .route("/api/v1/skills/{name}", get(skills_show))
          .route("/api/v1/memory", get(memory_list))
          .route("/api/v1/memory/stats", get(memory_stats))
          .route("/api/v1/personality", get(personality_get).put(personality_set))
          .route("/api/v1/channels", get(channels_list))
          .route("/api/v1/providers", get(providers_list))
          .route("/api/v1/providers/{id}/models", get(provider_models))
          .route("/api/v1/providers/{id}/models/refresh", post(provider_models_refresh))
  }
  ```
- Each handler's request/response shape lives in the same file — read each
  handler fn (e.g. `sessions_list`, `agent_chat`) for its input (path/query/body)
  and output JSON. Auth: routes are guarded by `check_auth` (bearer/pairing);
  only `version` and `auth/info` are intentionally public (confirm:
  `grep -n "check_auth\|is_authenticated" src/gateway/api_v1.rs`).
- Existing docs: `docs/reference/api-v1-streaming.md` (streaming only). The docs
  hub is `docs/README.md` + `docs/SUMMARY.md`; the reference section is
  `docs/reference/`. This is an English-only doc system (per CLAUDE.md) — do NOT
  promise translations.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| List routes (source of truth) | `grep -n "\.route(" src/gateway/api_v1.rs` | the 20 `.route` lines |
| Markdown lint (if configured) | the repo's markdown lint (see `.github/workflows/ci-run.yml` docs-quality job) | exit 0 |
| (Step 3, optional) API tests | `cargo test --test api_v1` | pass |

## Scope

**In scope**:
- `docs/reference/api-v1.md` — the new full reference (all 22 routes: method,
  path, auth requirement, request shape, response shape, status codes).
- `docs/SUMMARY.md` and `docs/README.md` — add a nav link to the new reference
  (keep nav concise, non-duplicative).
- (Optional, Step 3) `tests/api_v1.rs` — a contract test using plan 013's
  `spawn_test_gateway()` harness.

**Out of scope** (do NOT touch):
- The handlers themselves — this documents existing behavior; it does not change
  it. If documenting reveals a bug or an inconsistent shape, STOP and report as a
  finding.
- Adding new routes or versioning schemes (`/api/v2`) — out of scope.
- Non-English docs — the system is English-only.

## Git workflow

- Branch: `advisor/019-api-v1-reference-docs`
- Commit per logical unit (reference page, then nav, then optional tests).
  Messages e.g. `docs(reference): add full /api/v1 contract reference`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Author the reference from the source of truth

For each of the 20 `.route(...)` lines (some register two methods — e.g.
`sessions/{id}` is GET+DELETE, `personality` is GET+PUT), read the handler and
document:
- Method + path (with path params).
- Auth: required (bearer/pairing) or public (`version`, `auth/info`).
- Request: path params, query params, JSON body shape (field names + types).
- Response: JSON shape + notable status codes (200, 401 unauth, 404, 409).
- Cross-reference `docs/reference/api-v1-streaming.md` for `agent/chat`'s
  streaming behavior instead of duplicating it.

Structure it as a scannable reference (one section per resource group: sessions,
memory, skills, providers, agent, approvals, meta). Use neutral placeholders in
examples (no real tokens/paths).

**Verify**: `grep -c "^### " docs/reference/api-v1.md` ≥ 20 (roughly one heading
per route/method); every `.route` path in `api_v1.rs` appears in the doc:
```bash
for p in $(grep -oE '"/api/v1[^"]*"' src/gateway/api_v1.rs | tr -d '"' | sort -u); do
  grep -q "$p" docs/reference/api-v1.md || echo "MISSING: $p"
done
```
→ prints no `MISSING:` lines.

### Step 2: Wire navigation

Add a link to `docs/reference/api-v1.md` from `docs/SUMMARY.md` and the reference
index / `docs/README.md`, next to the existing streaming note. Keep nav concise.

**Verify**: `grep -rn "api-v1.md" docs/SUMMARY.md docs/README.md` → the link
exists; relative path resolves (`ls docs/reference/api-v1.md`).

### Step 3 (optional — requires plan 013): Test-enforce the contract

If plan 013's `spawn_test_gateway()` harness exists, add `tests/api_v1.rs` that,
for a representative subset of routes, asserts: unauth → 401 on a gated route;
auth → 200 with a body matching the documented shape (assert on key fields, not
byte-exact). This makes the doc a tested contract, not aspirational. If plan 013
has NOT landed, skip this step and note the dependency in the doc's header
("contract not yet test-enforced — see plan 013").

**Verify**: `cargo test --test api_v1` → pass (only if attempted).

## Test plan

- The doc-vs-source completeness check in Step 1 is the primary automated gate.
- Step 3's contract tests (optional) enforce auth + shape for a subset.
- Markdown lint / link check via the docs-quality CI job (run locally if the
  command is available).

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `docs/reference/api-v1.md` exists and every `/api/v1/*` path from
      `api_v1.rs` appears in it (the Step-1 loop prints no `MISSING:` lines)
- [ ] Auth requirement (gated vs public) is stated for every route
- [ ] `docs/SUMMARY.md` and `docs/README.md` link to the new page; the link resolves
- [ ] No real credentials/tokens/paths in examples (neutral placeholders only)
- [ ] (If Step 3 attempted) `cargo test --test api_v1` passes
- [ ] Only in-scope files modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- A handler's actual request/response shape can't be determined from the code
  without running it — document what's certain and flag the rest as "verify";
  don't invent a shape.
- Documenting reveals an inconsistency (e.g. two routes returning different
  shapes for the same concept, or a gated route that isn't actually gated) — STOP
  and report it as a finding rather than papering over it in docs.
- The route list has grown/changed substantially since `4d35107` — reconcile
  against the live router and note the delta.

## Maintenance notes

- Publishing a contract creates a stability obligation on these routes — say so
  in the doc header. Pair with plan 013 so the contract is enforced.
- Consider a follow-up to generate an OpenAPI spec from the router (deferred):
  it would let the contract test and the docs derive from one source. Note it.
- Every new `/api/v1` route added in future must be added to this reference (and,
  once plan 013 lands, get a contract test) — state that expectation in
  `docs/contributing/` if a checklist exists there.
