# Plan 262: [claw-ui] Fix config-panel load/error states and stale snapshots

> **REPO: claw-ui** (separate Next.js repo at `/home/sulthannauval/project/rantai/claw-ui`), NOT RantAIClaw. All paths below are relative to the claw-ui repo root.
>
> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` (in the RantAIClaw repo) when done.
>
> **Drift check (run first)**: in the claw-ui repo, `git diff --stat -- src/components/ops/config-panel.tsx src/components/ops/shared.tsx src/components/console/console-shell.tsx src/lib/console.ts`

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug (frontend UX)
- **Planned at**: RantAIClaw commit `0e5fcc9`, 2026-08-27

## Why this matters

The Config panel writes against config it may never have read, and shows stale values after a save:

- **L1** The temperature card renders unconditionally — no `cfg.loading`/`cfg.error` guard, Save always enabled. On a failed `GET /config` the operator sees an empty box (indistinguishable from "unset") with a live Save, and writes against a config they never read.
- **L2** The `PanelFrame` in the (collapsed) raw dump omits the `loaded` prop, so a post-save refresh 502 blanks the whole panel — a SUCCESSFUL save presents as a load error. The prop's doc (`shared.tsx`) documents exactly this.
- **L3** Temperature (right rail) + MCP nav badge are load-time snapshots never re-read, so a saved temperature keeps showing the pre-edit value all session — the same class `SKILLS_CHANGED`/`PERSONA_CHANGED` were built to fix.
- **L9** Unsaved edits are discarded on route switch with no warning.

## Current state (confirm before editing)

- `src/components/ops/config-panel.tsx`:
  ```tsx
  const cfg = useAsync(() => api.config(), []);           // :15
  ...
  <Card className="space-y-3 p-4">                        // :42 — renders unconditionally
    <Input value={temp} ... />                            // :47 — no min/max, no loading guard
    <Button size="sm" onClick={save} disabled={busy}>     // :55 — Save gated only by `busy`
  ...
  {showRaw && (
    <PanelFrame loading={cfg.loading} error={cfg.error} onRefresh={cfg.refresh}>  // :73 — no `loaded`
  ```
- `src/components/ops/shared.tsx:74-83` — the `loaded` prop doc: without it "any error blanked the whole panel — which made the most likely outcome of a successful save an error screen". `channels-panel.tsx:193`, `status-panel.tsx:33` pass it correctly.
- `src/lib/console.ts:63-75` — `SKILLS_CHANGED`/`PERSONA_CHANGED` event constants + docs; wired at `console-shell.tsx:332-335,352-355`, dispatched from `persona-panel.tsx:76`, `skills-panel.tsx:112`. The visibility-gated poll at `console-shell.tsx:396-420` covers `autonomy` only. Temperature seed: `console-shell.tsx:374-384`; consumers `right-panel.tsx:53-54` (temp), `console-shell.tsx:721` (MCP badge).
- `src/components/console/ops-view.tsx:19-32,55` — `PANELS[route]` swaps element type on route change, unmounting panel state.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Build | `npx next build` (in claw-ui) | build succeeds, no type errors |
| Drive | agent-browser against the running console | Config panel behaves per Test plan |

NOTE: this repo has no eslint config; verify via `next build` + a browser drive, not lint. Do NOT trust `rtk next build` output — run real `next build` and, if checking a bundle, grep the built output.

## Scope

**In scope (claw-ui)**: `src/components/ops/config-panel.tsx`, `src/components/ops/shared.tsx` (only if making `loaded` required), `src/components/console/console-shell.tsx`, `src/lib/console.ts`, optionally `src/components/ops/ops-view.tsx` (L9 dirty-guard).
**Out of scope**: the server redaction (RantAIClaw plan 232); provider-secret editing (plan 263); API error mapping (plan 264).

## Git workflow

- Branch (in claw-ui): `fix/config-panel-load-states`
- Message e.g. `fix: guard config panel load/error states and invalidate stale snapshots`
- Do NOT push/PR unless instructed.

## Steps

### Step 1 (L1+L2): guard the temperature card and pass `loaded`

Wrap the sampling card in the same `PanelFrame` (or an equivalent loading/error guard) and disable Save while `cfg.loading || (cfg.error && !cfg.loaded)`. Pass `loaded={cfg.loaded}` to the raw-dump `PanelFrame` (`:73`) and to the other two panels the finding names (tools-panel `:101`, providers-panel `:151`). Consider making `loaded` a required prop so new panels can't regress.

**Verify**: `npx next build` succeeds; drive: with the gateway stopped, the Config panel shows a load/error state and Save is disabled (not an empty box with a live Save).

### Step 2 (L3): add a `CONFIG_CHANGED` event and re-read on it

Add `CONFIG_CHANGED` (`rantaiclaw:config-changed`) to `src/lib/console.ts` (mirror `SKILLS_CHANGED`), dispatch it from `config-panel.tsx` `save()` and from mcp-panel add/remove, and listen in `console-shell.tsx` to re-run the `api.config()` seed block (temperature + MCP count).

**Verify**: drive: save a temperature; the right-rail readout updates without a full reload; add/remove an MCP server; the nav badge updates.

### Step 3 (L9, optional): warn on unsaved edits at route switch

Track a `dirty` flag per panel and confirm before a route change (or lift the draft into a shell-level store keyed by route). Scope the guard to genuinely dirty fields so it isn't intrusive.

**Verify**: drive: type in the temperature box, click another rail item; a confirm appears (or the draft survives the remount).

## Test plan

- No unit-test harness assumed; verify via `next build` + agent-browser drive:
  - gateway down → Config panel shows load/error, Save disabled (L1/L2).
  - save temperature → right rail updates same session (L3).
  - mcp add/remove → nav badge updates (L3).
  - unsaved edit + route switch → warned/preserved (L9).
- If a component test setup exists (`*.test.tsx` present), add a test for the Save-disabled-while-loading behavior.

## Done criteria

- [ ] `npx next build` succeeds with no type errors
- [ ] Config panel shows a load/error state and disables Save when `GET /config` fails (browser-verified)
- [ ] a saved temperature / mcp change updates the rail/badge same session (browser-verified)
- [ ] `git status` (claw-ui) shows only in-scope files
- [ ] `plans/README.md` (RantAIClaw) row updated

## STOP conditions

- Making `loaded` required breaks other panels that don't pass it — either pass it everywhere in this PR or keep it optional and note the follow-up.
- The `CONFIG_CHANGED` listener causes a reload loop (dispatch → refetch → dispatch) — ensure the seed block doesn't re-dispatch; report if it does.

## Maintenance notes

- Reviewer: confirm a gateway-down Config panel no longer looks like "temperature unset with a live Save", and that a saved value updates the rail.
- Pattern to reuse: the `SKILLS_CHANGED`/`PERSONA_CHANGED` CustomEvent invalidation — this plan extends it to config/mcp.
