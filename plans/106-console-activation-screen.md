# Plan 106: claw-ui: activation screen, and stop rendering broken sub-panels

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- (claw-ui repo) src/components/ops/`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: 103, 104
- **Category**: feature
- **Planned at**: commit `2ca7e59`, 2026-08-10

> **Repo note**: this plan targets the **separate** `claw-ui` repository at
> `../claw-ui` (Next.js). Its version is independent — currently `v0.3.16`.
> Verification uses `npx next build` / `npx vitest run`, not cargo.

## Why this matters

Two problems, one screen.

**The panel stacks broken sub-panels when the KB is not usable.**
`KbPanel` renders the settings card, the Documents/Graph switch, and the
list/graph body unconditionally — `kb-panel.tsx:88-129`. `KbList` fires
`api.kbGroups()` on mount (`:74`), which returns 503 when the KB is off or has
no key. So an operator with no KB configured sees the "add an API key" card
**plus two error panels underneath it**.

**The graph cannot say why it is empty.** `deriveGraphState`
(`graph-lens-helpers.ts:21-23`) only knows `disabled` via
`cap.intelligence_enabled`. On a 503 there is no `cap` at all, so it falls
through to a raw error string instead of "the Knowledge Base is off".

The operator asked for a login-shaped model: when the KB is off the panel
should be an activation screen, and turning it off should keep the key.

## Current state (verified at 2ca7e59)

- `KbPanel` — `kb-panel.tsx:73-131`
- `KnowledgeSettingsCard` — `knowledge-settings-card.tsx`, 159 lines; already
  handles the not-configured and env-managed cases well and must be built on,
  not replaced
- `Clear` today wipes both keys — `knowledge-settings-card.tsx:52-64`
- Nav entry is unconditional — `console.ts:57`; per the agreed design it stays
  visible and the panel becomes the activation screen
- `api.getKnowledge()` / `api.setKnowledge()` — `api.ts:381-389`

## Scope

**In scope**: the activation screen, gating the sub-panels, the deactivate
action, and the `no-credential` graph state.

**Out of scope**: nav visibility (stays as-is by decision), and the drawer /
count fixes (plan 111).

## Git workflow

```bash
cd ../claw-ui && git switch -c feat/kb-activation-screen
```

## Steps

### Step 1: Extend the types

`types.ts` — add `enabled: boolean` to `KnowledgeStatus`, and add
`credential_configured`, `graphrag_enabled`, `resolution` to `KbCapability`
(plan 097 adds them server-side; make the fields optional so the console still
works against an older gateway).

### Step 2: Gate the panel body

```tsx
export function KbPanel() {
  const status = useAsync(() => api.getKnowledge(), []);
  // Do not mount the library until the KB can actually answer. KbList fetches
  // on mount and a 503 renders an error panel under the activation card.
  if (status.loading) return null;
  if (!status.data?.enabled) {
    return <KnowledgeSettingsCard onActivated={status.refresh} />;
  }
  // ... existing segmented control + KbList/GraphLens
}
```

Keep `KnowledgeSettingsCard` mounted in the enabled branch too, as the compact
status row it already renders.

**Verify**: with the KB off, exactly one card renders and the browser network
tab shows **no** `kb/groups` request.

### Step 3: Make the card an activation screen

Three states, driven by `enabled` and `embedding_configured`:

| enabled | key | Screen |
|---|---|---|
| false | no | "Activate Knowledge Base" + key inputs + Activate |
| false | yes | "Knowledge Base is off" + Activate button + "Remove key" |
| true | yes | current compact status row + Deactivate + Edit + Remove key |

Critical: **Deactivate is not Clear.** Deactivate sends
`{"enabled": false}` and keeps the key. Keep the existing destructive Clear
behind a separate "Remove key" action with its existing `ConfirmModal`, and
reword its description — it currently says search will stop working, which is
now what Deactivate does.

Surface the 400 from plan 103's probe inline on the key input, not only as a
toast — a rejected key is a form error.

**Verify**: activate, deactivate, activate again without re-entering the key.

### Step 4: Add the `no-credential` graph state

`graph-lens-helpers.ts`:

```ts
export type GraphState = "loading" | "disabled" | "no-credential" | "empty" | "ready";

export function deriveGraphState(cap, corpusEntities, loading, hasData): GraphState {
  if (loading && !hasData) return "loading";
  if (cap && !cap.intelligence_enabled) return "disabled";
  if (cap && cap.intelligence_enabled && cap.credential_configured === false)
    return "no-credential";
  return (corpusEntities ?? 0) === 0 ? "empty" : "ready";
}
```

Render a distinct hint for it in `graph-lens.tsx:147-175`: extraction is on but
no credential resolves — add one in Knowledge Base settings. That replaces a
dead-end "set `KB_INTELLIGENCE_ENABLED`" instruction with an action the operator
can actually take.

There are **three** locations carrying that dead-end instruction, not one —
fix all of them in the same pass or the console contradicts itself:

1. `graph-lens.tsx:147-161` (the disabled state)
2. `doc-intelligence-drawer.tsx:99-101` (plan 111 also touches this — coordinate)
3. `knowledge-graph.tsx:321-325` — the renderer's own zero-node `EmptyState`,
   found in the final audit sweep; easy to miss because it only shows when a
   caller renders the component despite zero nodes.

### Step 5: Tests

`graph-lens-helpers.ts` has no test file. Add one covering all five states —
it is a pure function and it is the only thing pinning the console's honesty
about why a graph is empty.

## Test plan

```bash
cd ../claw-ui
npx vitest run
npx next build
```

Drive the real console (per the house rule: a green build is not evidence):

```bash
npx next start -p 3939
```

- KB off → one activation card, no `kb/groups` request in the network tab
- enter a bad key → inline error, nothing saved
- enter a good key → activates, library appears
- Deactivate → activation card with "Activate" (key retained), reactivate in
  one click
- Remove key → confirm modal, then the empty-state card

## Done criteria

- No stacked error panels in any KB state.
- Deactivate keeps the key; Remove key is separate and confirmed.
- The graph distinguishes off / no-credential / empty / ready.

## STOP conditions

- The gateway does not yet return `enabled` (plan 103 unlanded) — the panel
  would read `undefined` and gate everything off. Land 103 first.
- `next build` passes but the browser shows a stale bundle: check
  `stat -c %y .next/BUILD_ID` before trusting a served page.
