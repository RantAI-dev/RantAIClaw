# Plan 209: claw-ui Tools panel — render the catalog from the backend, send the Manual wildcard, and read the preset from status

> **Executor instructions**: Follow step by step. Run every verification
> command and confirm the expected result. On a "STOP condition", stop and
> report. When done, update this plan's status row in `plans/README.md`.
>
> This plan is in the **claw-ui** repo: `/home/sulthannauval/project/rantai/claw-ui`.
>
> **Drift check (run first)**, in the claw-ui repo:
> `git -C /home/sulthannauval/project/rantai/claw-ui diff --stat -- src/lib/console.ts src/components/ops/tools-panel.tsx src/lib/api.ts`

## Status

- **Priority**: P2 (security-adjacent — the console mislabels/under-gates autonomy)
- **Effort**: M
- **Risk**: MED
- **Depends on**: **205** (backend Manual = wildcard `["*"]`; and a tools-catalog
  endpoint — add it in this plan's Step 1 if 205 did not)
- **Category**: bug / feature
- **Planned at**: backend commit `bba8e1d`, 2026-08-20

## Why this matters

The console's Tools & Autonomy panel and its 4-rung autonomy ladder are built
from a hardcoded `BUILTIN_TOOLS` list (9 names, 3 of them phantom) and infer the
active rung from `always_ask` length. Three defects follow:

1. **Manual under-gates.** `rungToAutonomyPayload("manual")` sends
   `always_ask: BUILTIN_TOOLS` (9 names). After plan 205 the backend Manual is a
   wildcard that prompts for every tool; the console must send the same wildcard,
   or selecting "Manual" from the console still leaves ~40 tools ungated.
2. **Mislabeling.** `levelToRung` returns `"manual"` for **any** non-empty
   `always_ask`. The default config ships `always_ask = ["ssh","pty"]`, so a
   fresh install renders the Manual rung ("Safest") as active though only two
   tools are forced. And a Manual→Smart toggle sends `always_ask: []`, silently
   dropping the `ssh`/`pty` force-prompt.
3. **Phantom per-tool controls.** The per-tool switches render `web_search`,
   `send_message`, `cron_schedule` — names no backend gate consults — so toggling
   them writes `auto_approve` entries that do nothing.

## Current state

### The rung mapping — `claw-ui/src/lib/console.ts:306-329`

```ts
export function rungToAutonomyPayload(rung: string): { level: string; always_ask?: string[] } {
  switch (rung) {
    case "manual": return { level: "supervised", always_ask: BUILTIN_TOOLS };  // 9 names
    case "strict": return { level: "readonly" };
    case "off":    return { level: "full" };
    case "smart":
    default:       return { level: "supervised", always_ask: [] };
  }
}

export function levelToRung(level, alwaysAskCount = 0): string {
  const l = (level || "").toLowerCase().replace(/[_\-\s]/g, "");
  if (l === "readonly") return "strict";
  if (l === "full") return "off";
  return alwaysAskCount > 0 ? "manual" : "smart";   // <-- infers from count, not preset
}
```

### `BUILTIN_TOOLS` phantom names — `claw-ui/src/lib/console.ts:291-301`
(`web_search`, `send_message`, `cron_schedule` — see plan 205 for the backend
half.)

### The backend already exposes the true preset

`GET /api/v1/status` returns `autonomy_preset` (the backend computes it via
`preset_for_autonomy`), so the console can read the active preset directly
instead of inferring it from `always_ask` length.

## The fix

### Step 1 — a tools-catalog endpoint (if plan 205 did not add one)

Add a backend route `GET /api/v1/tools` (bearer-gated, in `src/gateway/api_v1.rs`)
that returns the registered tool names (from the same registry the gateway
builds via `all_tools_with_runtime(...)` at `src/gateway/mod.rs:527` — NOT a
bare `all_tools(...)`) with a display label. This is the single source the
console renders from. If plan 205 already added such an endpoint, consume it.

### Step 2 — Manual sends the wildcard

`rungToAutonomyPayload("manual")` → `{ level: "supervised", always_ask: ["*"] }`,
matching the backend sentinel from plan 205. Smart stays `always_ask: []` but
must **not** be the mechanism that drops `ssh`/`pty` — see Step 4.

### Step 3 — render per-tool controls from the catalog endpoint

Replace `BUILTIN_TOOLS` as the source of the per-tool switches with the
`GET /api/v1/tools` response. Keep an icon/label lookup for known tools; fall
back to the raw name. This removes the phantom rows and shows the real ~50-tool
surface (paginate/group if long — group by builtin / skill / MCP if the endpoint
carries a source).

### Step 4 — read the active rung from the preset, not `always_ask` length

Replace the `alwaysAskCount`-based inference in `levelToRung` (`console.ts:324-329`)
with a new helper — call it `rungFromPreset(preset: string)` (this name is NOT
in the codebase yet; you are adding it) — that maps the backend
`autonomy_preset` string directly: `manual`→manual, `smart`→smart,
`strict`→strict, `off`→off. Feed it the `autonomy_preset` field from
`GET /api/v1/status` (`src/gateway/api_v1.rs:342` — confirmed present). Keep the
old `levelToRung` level-based path only as a fallback when the preset is absent.
Update every caller that currently calls `levelToRung(level, alwaysAskCount)` to
call `rungFromPreset(preset)` when the preset is available. This fixes the
"default shows Manual" mislabel and makes a Manual→Smart switch a deliberate
preset change rather than an `always_ask`-clearing side effect.

## Files

- **In scope (claw-ui)**: `src/lib/console.ts` (`rungToAutonomyPayload`,
  `levelToRung`, `BUILTIN_TOOLS`), `src/components/ops/tools-panel.tsx` (render
  from the endpoint), `src/lib/api.ts` (add the `tools()` fetcher).
- **In scope (backend, only if 205 didn't)**: `src/gateway/api_v1.rs` (the
  `/tools` route).
- **Out of scope**: the editable safety flags + cost card (plan 213 — same
  panel, different concern; coordinate so they don't conflict), the backend
  Manual semantics (plan 205).

## STOP conditions

- If plan 205 has NOT landed (backend Manual is still the 9-name append, not the
  wildcard), STOP the Step 2 change — sending `["*"]` to a backend that treats
  `"*"` as a literal tool name would gate nothing. Land 205 first, or coordinate
  so both merge together.
- If `GET /api/v1/status` does not actually carry `autonomy_preset` in the
  running build, verify before keying Step 4 on it.

## Done criteria

1. `bun run build` (or the repo's build script) succeeds in the claw-ui repo.
2. `bun run test` (vitest) passes, including new tests for the console lib.
   **These assume plan 205 has landed** (backend Manual = wildcard `["*"]`) — see
   STOP condition #1; do not merge this ahead of 205. `rungFromPreset` is the
   helper you add in Step 4.

```ts
// console.test.ts
test("manual rung sends the prompt-everything wildcard", () => {
  expect(rungToAutonomyPayload("manual").always_ask).toEqual(["*"]);
});
test("rung is read from the preset, not always_ask length", () => {
  // default config: always_ask has ssh/pty but preset is 'smart' or 'supervised'
  expect(rungFromPreset("smart")).toBe("smart");
  expect(rungFromPreset("manual")).toBe("manual");
});
```

3. Manual/behavioral (drive the console against a running gateway per the repo's
   verification norm): selecting **Manual** results in `needs_approval` prompting
   for a non-builtin tool (e.g. trigger an `http_request` tool turn and confirm
   the approval prompt appears); the panel's per-tool list shows real tool names
   (no `send_message`); a fresh install shows the correct rung, not "Manual" by
   default.

## Test plan

- Unit: `console.ts` mapping functions (vitest), as above.
- Integration/manual: this repo verifies via `next build` + a browser/console
  drive against a live gateway (see `plans/README` claw-ui notes). Confirm the
  approval actually fires for a non-builtin tool under Manual — a unit test
  cannot prove the end-to-end gate.

## Risk & rollback

- **Risk**: MED — this changes what "Manual" sends and how the rung is displayed;
  it is coupled to backend plan 205. Cut/verify the backend first.
- **Rollback**: revert the claw-ui commit; the backend (205) stands alone.

## Maintenance note

Rendering from the `/tools` endpoint and reading the preset from `/status`
removes two hardcoded catalogs from the frontend — the same drift class that
produced the phantom names and the mislabel. Keep the console free of a local
tool list; the backend registry is the single source.
