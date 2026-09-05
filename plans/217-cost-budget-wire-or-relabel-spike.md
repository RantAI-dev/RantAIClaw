# Plan 217 (SPIKE): Cost caps are configured but enforced nowhere — wire `CostTracker` or delete the field and relabel

> **Executor instructions**: This is a DECISION spike. Choose an option, record
> it, and implement that option (Option B is directly executable; Option A
> produces a follow-up plan). On a "STOP condition", stop and report. When done,
> update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/security/policy.rs src/cost/tracker.rs src/cost/mod.rs src/config/schema.rs src/agent/loop_.rs`

## Status

- **Priority**: P2 (trust — the console cost cap is a control that does nothing)
- **Effort**: M (A) / S (B)
- **Risk**: MED (A: a real cost gate can start refusing runs)
- **Depends on**: none
- **Category**: security / direction (spike)
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

The Tools & Autonomy panel's "cost / day ($)" input saves
`max_cost_per_day_cents`, the gateway persists it, and it is carried in
`PolicyFields` — but **no code path reads it to block anything**. The schema's
own migration note says it "is tracked but not enforced". The one real budget
engine, `CostTracker::check_budget` (`src/cost/tracker.rs:51`), reads a
**different** field (`[cost].daily_limit_usd`) and is **never constructed**
(zero `CostTracker::new` call sites outside `src/cost/`). So there are two
parallel cost-budget concepts with near-identical names, one inert and one dead.

An operator who sets a $5/day ceiling gets a success toast and believes
autonomous runs are financially bounded. They are not. This is the highest
trust-damage item on the Tools & Autonomy surface because the setting most
likely handed to a non-technical owner is the one that lies.

Note: real enforcement also needs real token accounting, which is currently a
hardcoded zero (`empty_usage()` in `src/agent/agent.rs`) — so Option A depends
on that being wired (tracked separately as a memory/usage finding). Factor that
into the decision.

## Current state

- `src/security/policy.rs:91,112` — `max_cost_per_day_cents` carried, never read
  to gate. The round-trip test at `policy.rs:1219-1220` has a comment admitting
  "No production reader today".
- `src/cost/tracker.rs:13-172` — `CostTracker` (SQLite ledger, `check_budget`,
  `record_usage`) never constructed outside its own tests; `src/cost/mod.rs`
  carries `#[allow(unused_imports)]` on the re-exports.
- `src/gateway/config_api.rs` accepts `max_cost_per_day_cents` (write-back);
  `src/main.rs` displays it. No enforcement.

## The decision (produce this)

### Option A — wire one real cost budget

1. Pick the canonical field. Recommend keeping `[cost].daily_limit_usd`
   (the field `CostTracker` already reads) and treating `max_cost_per_day_cents`
   as an alias mapped onto it (or vice versa) — do not keep two live concepts.
2. Construct `CostTracker` at agent boot (alongside `SecurityPolicy`) and call
   `check_budget` before each model request, in the same place `is_rate_limited`
   is consulted.
3. This requires real token accounting (usage from provider responses threaded
   into the cost path) — it is inert today. Depend on / co-schedule that.
4. Default to **warn, don't block** for one release (a mis-priced model table
   would otherwise start refusing turns) with a kill switch, then flip to
   enforce.

This is M+ effort and depends on token accounting; scope it as its own
follow-up plan that this spike produces.

### Option B — delete the inert field and make the surface honest

If enforcement is not near-term (token accounting is still zero):

1. Remove `max_cost_per_day_cents` enforcement pretense: either delete the field
   (schema-version bump, migration) or have the config API reject/ignore it with
   a "not enforced" note.
2. Surface `[cost].daily_limit_usd` as the (still-dead-until-A) budget concept,
   clearly labeled roadmap, OR delete `src/cost/tracker.rs` too if neither will
   be wired soon.
3. Coordinate with claw-ui plan 213 Step 3: the cost card must not toast
   "updated" for an unenforced field.

Prefer A if token accounting is being wired anyway; otherwise B (stop the lie
now).

## Deliverable of this spike

- The decision (A or B) with rationale in the PR + `plans/README.md`.
- If A: a follow-up plan wiring `CostTracker` + the token-accounting dependency,
  with a warn-then-enforce rollout and tests (a run over budget is warned/blocked
  per the chosen mode).
- If B: the field-removal/relabel PR (directly executable) + the claw-ui cost-card
  coordination (plan 213).

## Files (for the eventual implementation)

- `src/security/policy.rs` (field), `src/cost/*` (tracker), `src/agent/loop_.rs`
  / `src/agent/agent.rs` (the model-request path + token accounting),
  `src/config/schema.rs` (canonical field + migration), `src/gateway/config_api.rs`
  (accept/reject), claw-ui `tools-panel.tsx` (plan 213).

## STOP conditions

- Do not wire a hard cost block without the warn-first rollout — a pricing-table
  error would refuse legitimate runs.
- Do not keep BOTH `max_cost_per_day_cents` and `[cost].daily_limit_usd` as live
  concepts; collapse to one.
- Real enforcement is blocked on token accounting being non-zero; if that is not
  in scope, choose Option B.

## Done criteria (spike)

- Decision recorded; either the follow-up plan (A) exists or the relabel/delete
  PR (B) is opened and passes `cargo fmt`/`clippy`/`cargo test`.

## Risk & rollback

- Spike is low-risk. Option A implementation is MED-risk (behavioral cost gate);
  Option B is LOW (removes/relabels an inert field, with a migration if deleted).

## Maintenance note

One budget concept, one enforced path, one honest label. The current state — two
similarly-named fields, one inert and one dead, plus a success toast — is exactly
the "control that lies" this spike exists to end.
