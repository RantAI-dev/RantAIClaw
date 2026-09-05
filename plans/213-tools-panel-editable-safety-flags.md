# Plan 213: claw-ui Tools panel — make the accepted safety flags editable and stop the cost card misrepresenting enforcement

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> This plan is in the **claw-ui** repo: `/home/sulthannauval/project/rantai/claw-ui`.
>
> **Drift check (run first)**, claw-ui repo:
> `git -C /home/sulthannauval/project/rantai/claw-ui diff --stat -- src/components/ops/tools-panel.tsx src/lib/api.ts`

## Status

- **Priority**: P2 (honesty + UX — panel can't edit the settings it's named for)
- **Effort**: M
- **Risk**: MED (forbidden-path / flag edits loosen safety gates)
- **Depends on**: 216 (the cost half is honest only once cost enforcement/label
  is decided) — the flag half is independent
- **Category**: bug / feature
- **Planned at**: backend commit `bba8e1d`, 2026-08-20

## Why this matters

`PUT /api/v1/config/autonomy` accepts 10 fields, but the "Tools & Autonomy ·
Permissions" panel can edit only ~6 of them. Four safety-relevant fields the
same endpoint accepts are rendered **display-only**, so an operator is sent to
the TUI/`config.toml` for exactly the settings the panel is named after:

- `block_high_risk_commands` — a display `StatTile`, and it defaults **OFF**;
  there is no control to turn the high-risk backstop on.
- `workspace_only` — display `StatTile`.
- `forbidden_paths` — badges under a header that literally says "(read-only)",
  yet the same PUT overwrites them (see plan 198 — the backend now enforces a
  floor, so editing them here is safe up to that floor).
- `require_approval_for_medium_risk` — appears only as footer prose, never sent.

Separately, the "Rate & cost caps" card groups an **enforced** cap
(`max_actions_per_hour`) with an **inert** one (`max_cost_per_day_cents`, see
plan 217) under one heading and one "updated" toast, presenting them as equally
trustworthy. It also shows only the configured cap, never live usage.

## Current state

### Display-only fields — `claw-ui/src/components/ops/tools-panel.tsx:132-267`

- `block_high_risk` / `workspace_only` as `StatTile` (`:134-143`).
- `forbidden_paths` badges under "(read-only)" (`:250-263`).
- `require_approval_for_medium_risk` as footer prose (`:267`).

### The endpoint accepts all of them — `src/gateway/config_api.rs:360-395`

`set_autonomy` accepts `block_high_risk_commands`, `workspace_only`,
`forbidden_paths`, `require_approval_for_medium_risk` (and the rest).

## The fix

### Step 1 — promote the two flags to switches

Render `block_high_risk_commands` and `require_approval_for_medium_risk` as
editable switches wired through the existing `patch({...})` helper the panel
already uses for other fields. This makes the high-risk backstop reachable
(it ships off) and the medium-risk gate adjustable.

### Step 2 — `workspace_only` switch + guarded `forbidden_paths` editor

Make `workspace_only` a switch. Turn `forbidden_paths` into a chip editor
(add/remove), removing the "(read-only)" label. Because these loosen safety
gates, add a confirmation on removal and — mirroring the allowlist drift check
the Channels panel does — re-read before write to avoid clobbering an external
change. Note: plan 198 gives the backend a non-removable floor, so the editor
cannot reduce protection below the baseline; surface that (e.g. show floor
entries as non-removable).

### Step 3 — honest cost/rate card

Split the "Rate & cost caps" card:

- "Rate cap (enforced)" for `max_actions_per_hour`, ideally showing live
  usage (actions this hour) if a usage read is available.
- Gate the cost control behind real enforcement: until plan 217 wires
  `max_cost_per_day_cents` (or relabels it), either hide the cost input or label
  it clearly "not yet enforced" and do not toast "updated" for it. Coordinate
  the final wording with plan 217's decision.

### Step 4 — coordinate with plan 209

Plan 209 also edits `tools-panel.tsx` (catalog + rung). Sequence the two so they
do not conflict; ideally land 209 first (it restructures the per-tool list), then
this plan adds the flag/path/cost controls.

## Files

- **In scope (claw-ui)**: `src/components/ops/tools-panel.tsx`, `src/lib/api.ts`
  (if a usage read is added).
- **Out of scope**: the backend `forbidden_paths` floor (plan 198 — land first),
  cost enforcement (plan 217), the catalog/rung (plan 209).

## STOP conditions

- If plan 198 (the `forbidden_paths` floor) has NOT landed, STOP the Step 2
  forbidden-path editor — without the floor, the editor lets an operator strip
  all path protection from the console. Land 198 first, or ship Step 1/3 only
  and defer Step 2.
- If plan 209 is mid-flight on the same file, coordinate/rebase to avoid a merge
  conflict.

## Done criteria

1. `bun run build` succeeds; `bun run test` passes with new tests for any added
   pure logic (e.g. the forbidden-path editor's add/remove reducer).
2. Behavioral (drive the console against a running gateway): toggling
   `block_high_risk_commands` on persists and the backend reports it enabled;
   editing `forbidden_paths` persists and cannot remove a floor entry; the cost
   input is either hidden or clearly labeled unenforced; the rate cap shows as
   enforced.

## Test plan

- Unit (vitest): the forbidden-path chip reducer and any patch-payload builder.
- Behavioral: the repo's console drive; confirm each promoted control round-trips
  through `GET /config`. A unit test can't prove the backend enforcement — verify
  `block_high_risk` actually blocks by driving a high-risk command turn.

## Risk & rollback

- **Risk**: MED — these controls loosen safety gates (forbidden_paths,
  medium-risk approval). The confirmation + the backend floor (198) bound the
  blast radius. The cost-card change is presentation-only.
- **Rollback**: revert the claw-ui commit.

## Maintenance note

The panel should be able to edit every field its endpoint accepts, or the field
should not be accepted — an accepted-but-display-only field is a latent
"the same PUT overwrites it" trap (that was the `forbidden_paths` "read-only"
lie). Keep the panel's editable set in sync with `set_autonomy`'s accepted set.
