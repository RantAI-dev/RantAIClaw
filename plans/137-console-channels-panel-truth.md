# Plan 137: The console's channels panel tells the truth about what it did

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **THIS PLAN CHANGES A DIFFERENT REPOSITORY.** All source paths below are
> relative to `/home/sulthannauval/project/rantai/claw-ui` (a separate Next.js
> repo). The plan file itself lives in the RantAIClaw repo because that is where
> this effort's plans are tracked. Do not modify anything under RantAIClaw.
>
> **Drift check (run first)**, from the claw-ui repo:
> `git diff --stat 585f702..HEAD -- src/components/ops/channels-panel.tsx src/hooks/use-async.ts src/components/ops/shared.tsx`
>
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged
> first. That is expected and is not a stop condition. Relocate by symbol name
> (function, constant, struct) and continue. STOP only if the *code itself*
> no longer matches the "Current state" excerpt semantically — i.e. the logic
> changed, not its position.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none (but see "Maintenance notes" — pairs with RantAIClaw plan 115)
- **Category**: bug
- **Planned at**: claw-ui commit `585f702`, 2026-08-12

## Why this matters

When an operator saves a Telegram allowlist, the console shows a green "Allowlist
updated" toast and then breaks. Three separate console defects turn a backend
restart into a silent failure:

1. Success is asserted before the effect is observable, and a refetch is fired
   immediately — racing a daemon restart the save itself triggered 750 ms later.
2. The gateway's "the runtime is reloading" notice is suppressed by an `else if`
   in exactly the two states where an operator most needs it.
3. A refresh failure blanks the whole panel, so the most likely outcome of a
   *successful* save is an error screen — indistinguishable, to the operator, from
   the save having failed.

There is also a fourth, quieter problem: the panel sends the entire allowlist as a
wholesale replacement built from a possibly-stale snapshot, so it silently revokes
anyone who self-onboarded via `/claim` since the panel was opened. The backend goes
out of its way to avoid clobbering that (it re-reads the freshest config under a
lock, with a comment saying so); the console defeats it.

After this plan: the panel reports what actually happened, shows the operator what
changed before it changes it, keeps its content on screen while the gateway
restarts, and reconnects on its own.

## Current state

`src/components/ops/channels-panel.tsx:143-155` — the save handler:

```tsx
  const saveAllowlist = async () => {
    setBusy(true);
    try {
      const r = await api.updateTelegramAllowlist(parseUsers());
      toast.success("Allowlist updated");
      notify(r);
      onReload();
    } catch (e) {
      toast.error(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };
```

`src/components/ops/channels-panel.tsx:121-124` — the suppressing `else if`:

```tsx
  const notify = (r: { warning?: string | null; note?: string }) => {
    if (r.warning) toast.warning(r.warning);
    else if (r.note) toast.message(r.note);
  };
```

The gateway sets `warning` when the allowlist is empty or contains `*`, and puts
the restart notice in `note`. So the two states an operator is most likely to be
in while editing are exactly the states where the restart notice disappears.

`src/components/ops/channels-panel.tsx:110-119` — the editor seeds from a derived
string with no dirty flag, and the payload is the whole box:

```tsx
  const savedAllowlist = allowedUsers.join(", ");
  React.useEffect(() => {
    if (connected) setUsers(savedAllowlist);
  }, [connected, savedAllowlist]);

  const parseUsers = () =>
    users
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean);
```

`src/lib/api.ts:50-66` — the fetch wrapper has no retry:

```ts
async function rc<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api/rc/${path}`, { … });
  const text = await res.text();
  const data = text ? JSON.parse(text) : null;
  if (!res.ok) { … throw new ApiError(…, res.status, data); }
  return data as T;
}
```

`src/components/ops/shared.tsx:82-98` — `PanelFrame` renders the error state
*instead of* its children whenever `error` is set, and `src/hooks/use-async.ts:22-23`
sets `error` on a refresh failure exactly as on an initial load.

`src/components/ops/channels-panel.tsx:53-55` — the post-save refetch is scheduled
3 s out, i.e. inside the restart window, and its timer handle is never cleared.

### Conventions to follow

- The repo already has the correct guarded async hook at
  `src/hooks/use-async-guarded.ts:15-28` (request-id token, used by one lens).
  Reuse it — do not write a third variant.
- Gateway connection state already exists: `src/hooks/use-gateway-status.ts`
  (`Connection = "connecting" | "online" | "offline"`), rendered as a badge at
  `src/components/console/console-shell.tsx:585` and a banner at
  `src/components/console/chat-pane.tsx:304`. Reuse that vocabulary.
- Toasts come from the existing `toast` import in this file; match its usage.

## Commands you will need

Run these **from `/home/sulthannauval/project/rantai/claw-ui`**.

| Purpose | Command | Expected on success |
|---|---|---|
| Typecheck | `bunx tsc --noEmit` | exit 0, no errors |
| Build | `bun run build` | exit 0 |
| Unit tests | `bunx vitest run` | all pass |

There is **no eslint config** in this repo, so `bun run lint` contributes nothing —
do not rely on it. Verification here is typecheck + build + driving the page.

## Scope

**In scope**:
- `src/components/ops/channels-panel.tsx`
- `src/hooks/use-async.ts` (port the request-id guard from `use-async-guarded.ts`)
- `src/components/ops/shared.tsx` (keep content mounted on a *refresh* failure)
- `src/lib/api.ts` — only to add a return type field if step 3 needs it

**Out of scope** (do NOT touch):
- Any other panel under `src/components/ops/` — several share the same stale-seed
  and optimistic-success shapes, but they are owned by plan 138. Changing
  `use-async.ts` will affect them; that is intended and is why 138 depends on this
  plan. Do not "fix" their call sites here.
- `src/hooks/use-gateway-status.ts` — read it, reuse it, do not modify it.
- `src/app/api/rc/**` and `src/proxy.ts` — the BFF Host allowlist is plan 138.
- Anything in the RantAIClaw repo. If the gateway response needs a new field,
  STOP and report rather than editing the backend.

## Git workflow

- Branch: `fix/console-channels-panel-truth` (in the claw-ui repo)
- Conventional commits, matching that repo's `git log` style, e.g.
  `fix(ops): stop the channels panel reporting success it cannot observe`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Stop suppressing the gateway's notice

Change `notify` so a `warning` and a `note` can both surface. The gateway sends
them for different reasons and neither implies the other:

```tsx
  const notify = (r: { warning?: string | null; note?: string }) => {
    if (r.warning) toast.warning(r.warning);
    if (r.note) toast.message(r.note);
  };
```

**Verify**: `bunx tsc --noEmit` → exit 0.

### Step 2: Show the operator what will change before it changes

Before POSTing, re-fetch the current config and compare the server's
`allowed_users` against the snapshot the editor was seeded from. If they differ,
do not save silently — surface the difference and require confirmation.

Concretely, in `saveAllowlist`:
- fetch fresh config via the existing config API the panel already uses,
- diff `fresh.allowed_users` against `allowedUsers` (the prop the editor was
  seeded from),
- if they differ, render the added/removed entries and ask the operator to confirm
  (reuse the existing `ConfirmModal` — it is already imported in this file for
  disconnect),
- on confirm, proceed with the operator's current text box contents.

This preserves the wholesale-replacement API contract while making the revocation
visible, which is the actual defect. Do not attempt to build a delta API — the
gateway endpoint takes a full list and changing it is out of scope.

**Verify**: `bunx tsc --noEmit` → exit 0; `bun run build` → exit 0.

### Step 3: Report what the server did, not what was requested

`updateTelegramAllowlist` returns `allowed_users` as a **count**
(`src/lib/types.ts:304`). Render it, so a mismatch between what the operator typed
and what the server stored is visible:

```tsx
      toast.success(`Allowlist updated — ${r.allowed_users} sender(s) allowed`);
```

**Verify**: `bunx tsc --noEmit` → exit 0.

### Step 4: Do not blank the panel when a refresh fails

In `src/components/ops/shared.tsx`, `PanelFrame` currently swaps children for the
error state whenever `error` is truthy. Distinguish an **initial load** failure
(no data yet — the error state is right) from a **refresh** failure (data is
already on screen — keep it, and show the error as a non-blocking strip).

`use-async.ts` already tracks enough state to tell these apart (it has a `loaded`
notion); if it does not expose it, add a boolean to its return value rather than
inferring from `data != null`.

**Verify**: `bunx vitest run` → all pass.

### Step 5: Give the shared async hook a request-id guard and clean up the timer

Port the `reqId` guard from `src/hooks/use-async-guarded.ts:15-28` into
`src/hooks/use-async.ts`, preserving the existing `refreshing` / `loaded`
semantics exactly. This makes an older in-flight response unable to overwrite a
newer one — the case an operator produces by hitting Refresh during a restart.

In `channels-panel.tsx`, store the 3-second settle timer in a ref and clear it on
unmount.

**Verify**: `bunx vitest run` → all pass; `bunx tsc --noEmit` → exit 0.

### Step 6: Give the panel a restarting/reconnecting state

Consume `use-gateway-status` in the channels panel. When a save has been made that
the gateway said it is reloading for (the `note` from step 1), render a
"Reloading the runtime…" state and poll until the gateway reports `online` again,
then refresh. While in that state, the panel keeps its content (step 4) and does
not present the outage as an error.

Keep the poll bounded — if the gateway does not come back within a reasonable
window, fall through to the existing error presentation with a message that names
the likely cause and the manual recovery (`systemctl --user reset-failed
rantaiclaw.service` then start).

**Verify**: `bun run build` → exit 0.

### Step 7: Drive it in a browser

Static checks cannot catch a race. Start the console against a running gateway and
exercise, in order:

1. Save an allowlist with a `*` entry — confirm **both** the warning and the
   restart notice appear (step 1).
2. Save an allowlist while a second identity has been added out of band — confirm
   the confirmation dialog lists the removal (step 2).
3. Save a change that triggers a restart — confirm the panel shows "Reloading…",
   keeps its content, and recovers on its own without a manual refresh (steps 4–6).
4. Hit Refresh twice rapidly during the restart — confirm the panel does not end
   up showing pre-save data (step 5).

Record what you observed for each. **A green build is not sufficient evidence for
this plan** — every defect it fixes is a timing behaviour that only shows up when
driven.

## Test plan

This repo currently **cannot test React components**: `vitest.config.mts` sets
`environment: "node"`, its `include` glob is `src/**/*.test.ts` (which does not
match `.test.tsx`), and there is no jsdom or testing-library dependency.
Standing up that harness is plan 138.

So for this plan, extract the pure decisions and test them at the lib layer, which
the repo already does well (see `src/lib/console.test.ts`):

- `allowlistDiff(seeded: string[], fresh: string[]) -> { added, removed }` — new
  pure function in `src/lib/`, tested with: identical lists, an out-of-band
  addition, an operator removal, and both at once.
- `shouldBlockOnError(hasData: boolean, error: unknown) -> boolean` — the step-4
  decision, tested for initial-load-failure vs refresh-failure.
- The `use-async` request-id guard: `src/hooks/use-async-guarded.ts` has no tests
  today; add a test that an older resolution does not overwrite a newer one, using
  the guarded hook's pure reducer if it has one, or by extracting it.

**Verify**: `bunx vitest run` → all pass, including the new tests.

## Done criteria

ALL must hold:

- [ ] `bunx tsc --noEmit` exits 0
- [ ] `bun run build` exits 0
- [ ] `bunx vitest run` passes, including the new lib-layer tests
- [ ] `grep -n 'else if (r.note)' src/components/ops/channels-panel.tsx` returns nothing
- [ ] The browser drive in step 7 was performed and all four observations recorded
      in the PR notes
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 137 updated (in the RantAIClaw repo)

## STOP conditions

Stop and report back (do not improvise) if:

- The gateway does not return a field that lets you distinguish "this save will
  restart the runtime" from "this save applied live". After RantAIClaw plan 115
  lands, the allowlist-only path stops restarting and the `note` text changes —
  if you are working before 115 lands, the note is the only signal and step 6 must
  key on it. If neither is available, stop rather than guessing.
- Changing `use-async.ts` breaks a panel outside your scope in a way you cannot
  fix without editing it. That is plan 138's territory; report the breakage
  instead of expanding scope.
- The browser drive shows the panel recovering *without* your change — that would
  mean the race is not reproducible here and the plan's premise needs rechecking
  before you ship a fix for it.

## Maintenance notes

- **Pairs with RantAIClaw plan 115.** That plan stops the gateway restarting for
  allowlist-only edits. Once it lands, step 6's reconnect state should only ever be
  reached on connect / token-change / disconnect. If it still fires on plain
  allowlist saves after 115 is deployed, one of the two plans did not take effect —
  check which, rather than adding more retry.
- **What a reviewer should scrutinise**: that step 4 genuinely distinguishes
  initial from refresh failures rather than checking `data != null` (which is
  wrong the first time a panel legitimately loads empty), and that step 2's
  confirmation cannot be skipped by an operator holding Enter.
- **Deliberately deferred to 138**: the same optimistic-success shape in the
  providers, MCP, skills and cron panels; the typed-error mapping; the BFF Host
  allowlist; and the component-test harness. They are separated because 138 needs
  the harness this plan does not, and keeping them apart keeps this one shippable.
