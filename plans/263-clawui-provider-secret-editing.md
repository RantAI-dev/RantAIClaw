# Plan 263: [claw-ui] Provider/secret editing robustness

> **REPO: claw-ui** (`/home/sulthannauval/project/rantai/claw-ui`), NOT RantAIClaw. Paths below are claw-ui-relative.
>
> **Executor instructions**: Follow step by step; verify each step; confirm each cited excerpt before editing; STOP-condition = stop and report. Update `plans/README.md` (RantAIClaw) when done.
>
> **Drift check (run first)**: in claw-ui, `git diff --stat -- src/components/ops/providers-panel.tsx src/components/ops/tools-panel.tsx src/lib/console.ts`

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW–MED (secret editing; a wrong change could clear a key unexpectedly)
- **Depends on**: none
- **Category**: bug (frontend)
- **Planned at**: RantAIClaw commit `0e5fcc9`, 2026-08-27

## Why this matters

- **L4** The console cannot clear `api_key`/`api_url` — the backend's clear-on-empty contract is unreachable because the panel guards `if (key.trim()||url.trim())` and sends `x.trim()||undefined`. A compromised key can't be revoked from the console; the only "fix" is overwriting with another key.
- **L5** Provider save is two non-atomic writes (`setConfigModel` then `setSecrets`) with the refresh calls inside the `try` after both awaits → if the second fails, the gateway has switched provider while the console still shows the old one and re-runs nothing.
- **L10** Autonomy rung buttons send partial deltas (`strict`/`off` omit `always_ask`) → `always_ask` residue persists under a `full` level; the Tools panel then mislabels every tool.

## Current state (confirm before editing)

- `src/components/ops/providers-panel.tsx`:
  - `:49-58` — `await api.setConfigModel({provider, model})` then, separately, `await api.setSecrets({api_key, api_url})`.
  - `:56-57` — `if (key.trim() || url.trim())` gates the secrets call; each field sent as `x.trim() || undefined` → an empty string never leaves the client.
  - `:66-68` — `setKey("")`, `secrets.refresh()`, `info.refresh()` inside the `try` AFTER both awaits (`:69-71` catch), so none run if the second call throws.
  - `:33` — mirrors an absent `api_url` into an empty field (so the operator sees an empty box Save then ignores).
  - Backend contract (RantAIClaw `src/gateway/config_api.rs:794-853`): a provided field sets (empty string CLEARS), omitted leaves untouched.
- `src/lib/console.ts:318-333` — `rungToAutonomyPayload` returns `{level:"readonly"}` for `strict` and `{level:"full"}` for `off`, both omitting `always_ask`; only `manual`/`smart` set it. `set_autonomy` (config_api) applies only present fields. Enforcement is safe (short-circuits on Full) — this is persisted residue + mislabeling, not a security hole.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Build | `npx next build` (claw-ui) | build succeeds |
| Drive | agent-browser against the console | per Test plan |

No eslint config; verify via `next build` + browser drive. Don't trust `rtk next build`.

## Scope

**In scope (claw-ui)**: `src/components/ops/providers-panel.tsx`, `src/lib/console.ts` (rung payloads). Possibly a shared `ConfirmModal` reuse (`knowledge-settings-card.tsx` uses one).
**Out of scope**: the backend clear-on-empty contract (works); config-panel load states (plan 262); API error mapping (plan 264).

## Git workflow

- Branch (claw-ui): `fix/provider-secret-editing`
- Message e.g. `fix: allow clearing provider secrets, make provider save resilient, send complete autonomy payloads`
- Do NOT push/PR unless instructed.

## Steps

### Step 1 (L4): explicit clear affordances

Add "Remove key" / "Reset base URL" buttons (behind the existing `ConfirmModal` pattern) that send `""` deliberately (the backend's clear signal), leaving blank-means-keep intact for the normal Save path. Do NOT change the blank-field default to send `""` (that would clear a key any time the user saves with the field blank).

**Verify**: drive: use "Remove key" → the key is cleared (the "key set" badge flips to unset); a normal Save with a blank field does NOT clear it.

### Step 2 (L5): make provider save resilient

Move `secrets.refresh()`/`info.refresh()` into a `finally` so the panel always re-reads server truth after a save attempt. Word the error toast to say which half landed (model switched vs secrets). (Optional longer-term: a single endpoint that accepts provider+model+key atomically — note as a follow-up, don't build here.)

**Verify**: drive: simulate a secrets failure (e.g. invalid input) after a provider switch → the panel re-reads and shows the actual server state, not the stale pre-save view.

### Step 3 (L10): send complete autonomy payloads

In `rungToAutonomyPayload` (`console.ts:318`), always send an explicit `always_ask` (`[]` for `strict` and `off`) so each rung writes a complete, self-consistent state.

**Verify**: drive: switch Manual → Off → the Tools panel no longer labels tools "always prompts (Manual)" under an Off rung.

## Test plan

- Verify via `next build` + agent-browser:
  - clear a key via the explicit button (L4); blank-Save does not clear (L4).
  - a failing secrets write after a provider switch → panel re-reads server truth (L5).
  - Manual→Off leaves no `always_ask` residue in the Tools panel (L10).
- If `*.test.ts(x)` harness exists, add a unit test for `rungToAutonomyPayload` asserting every rung includes `always_ask`.

## Done criteria

- [ ] `npx next build` succeeds
- [ ] a provider key can be cleared from the console (browser-verified); blank-Save does not clear (browser-verified)
- [ ] refreshes run in `finally` (`grep -n "finally" src/components/ops/providers-panel.tsx`)
- [ ] `rungToAutonomyPayload` sends `always_ask` for every rung (`grep`)
- [ ] `git status` (claw-ui) shows only in-scope files
- [ ] `plans/README.md` (RantAIClaw) row updated

## STOP conditions

- Adding an explicit `""` clear collides with the blank-means-keep default in a way that clears keys unexpectedly — the two must be distinct (explicit button vs blank Save); report if they can't be separated.
- `set_autonomy` rejects an empty `always_ask` after RantAIClaw plan 235's validation lands — coordinate: `[]` should be valid; report if it's rejected.

## Maintenance notes

- Reviewer: confirm blank-Save still means "keep" and only the explicit button clears (L4 is the subtle one).
- Interacts with RantAIClaw plans 232 (redaction) and 235 (autonomy validation) on the same endpoints.
