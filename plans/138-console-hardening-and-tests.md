# Plan 138: Console — autonomy rollback, typed errors, Host allowlist, component tests

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **THIS PLAN CHANGES A DIFFERENT REPOSITORY.** All source paths are relative to
> `/home/sulthannauval/project/rantai/claw-ui`. The plan file lives in the RantAIClaw
> repo because that is where this effort's plans are tracked. Do not modify anything
> under RantAIClaw.
>
> **Drift check (run first)**, from the claw-ui repo:
> `git diff --stat 585f702..HEAD -- src/components/console/ src/proxy.ts src/lib/api.ts vitest.config.mts`
>
> **Line numbers WILL have drifted** — plan 137 merges before this one. Relocate by
> symbol name and continue. STOP only if the *code itself* no longer matches the
> "Current state" excerpt semantically.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: MED
- **Depends on**: plans/137 (serialized over `src/hooks/use-async.ts` and the ops panels)
- **Category**: security
- **Planned at**: claw-ui commit `585f702`, 2026-08-12

## Why this matters

Plan 137 fixed the channels panel. This plan covers everything else the console gets
wrong about state it does not own — including two ways a security control moves
without the operator meaning it.

**Shift+Tab changes the agent's approval policy** from any non-editable focus. The
code's own comment names the hazard — "could flip autonomy by accident" — and then
guards only text inputs and dialogs. From a button, a link or the rail nav it still
cycles the rung, and one of the four rungs is "autonomous execution, no prompts".
The unconditional `preventDefault()` also breaks reverse-tab navigation everywhere.

**A failed autonomy write leaves the wrong rung on screen** and arms the staleness
guard that would have corrected it, so the console can display "Off" while the
gateway is on Manual — or the reverse, for an operator who believes they just locked
the agent down. Recovery waits on a 30-second poll that stops when the tab is hidden.

**The console never shows who can approve tool calls**, or whether chat approval is
switched off entirely: `approval_owners` and `autonomous_tools` appear nowhere in
`src/`.

**The BFF has no expected-Host allowlist**, so a rebound DNS name satisfies its only
same-origin check and the page's script can read the full config and issue privileged
writes, all signed with the gateway token.

And underneath all of it: **no React component in this repo is testable, let alone
tested.**

## Current state

`src/components/console/console-shell.tsx:400-424` — the binding, with its own warning:

```tsx
        // Shift+Tab is the universal "focus previous" key — do NOT hijack it while
        // the user is typing or a dialog is open (that would break reverse-tab and
        // could flip autonomy by accident). Only cycle from a non-editable context.
        …
        if (editable || document.querySelector('[role="dialog"]')) return;
        e.preventDefault();
```

`:348-361` — the optimistic write with no rollback, and the guard stamped in
`.finally()` so a **failed** write arms it; `:229` discards any read older than that
stamp; `:61` — recovery waits on `AUTONOMY_POLL_MS = 30000`, and `:320-340` stops the
poller while the tab is hidden.

`grep -rn 'approval_owners|autonomous_tools' src/` → **0 matches**, while the backend
gates `/allow` on `approval_owners` (`RantAIClaw/src/channels/approval_relay.rs:57-71`)
and `[channels_config] autonomous_tools = true` bypasses chat approval entirely
(`RantAIClaw/src/gateway/channel_approval.rs:22`).

`src/lib/request-origin.ts:56`, `:69`:

```ts
  if (secFetchSite) return secFetchSite !== "same-origin" && secFetchSite !== "none";
  …
    return originHost !== host;
```

Avoiding a fixed self-origin is deliberate and correct — `req.nextUrl.origin` is the
bind address in the standalone server — but the replacement compares the Origin
against the request's own Host. `src/proxy.ts:48-57` is the only gate when console
login is off, which the file notes is the default; `src/app/api/rc/[...path]/route.ts:27`
attaches the gateway bearer token to every forwarded request.

`src/lib/api.ts:38-48` — `ApiError` carries `status` and the parsed `body`, and its
comment says callers can act on it. Two of roughly twenty-two mutation handlers do;
the rest flatten it to `.message`.

`vitest.config.mts:6-7` — `environment: "node"` and `include: ["src/**/*.test.ts"]`,
which does not match `.test.tsx`. `grep -c 'jsdom\|happy-dom\|@testing-library'
package.json` → **0**. `find src -name '*.test.tsx'` → **0**. Of 19 modules under
`src/components/ops/`, exactly one has a test, and it is a pure helper.

`src/components/ops/config-panel.tsx:69` — the label "Show full config (secrets
redacted)" over a backend suffix heuristic that cannot cover `mcp_servers.*.env`.

`src/components/ops/skills-panel.tsx:132-140` — `toggle()` never sets the busy flag, so
the control's `disabled` never engages during the in-flight write.
`src/components/ops/cron-panel.tsx:339` and `src/lib/api.ts:198-202` — the
`approved=true` parameter exists and no caller passes it, so a gated job is unrunnable
from the console; `cron-panel.tsx:138-142` truncates the refusal at 200 characters.

`README.md:25` — "set `RANTAICLAW_UI_PASSWORD` to enable"; `grep -rn
'RANTAICLAW_UI_PASSWORD' src/` → **0**. `README.md:52` — `bun run pair`, which is not
a script. `docs/auth.md:61-71` is accurate and contradicts the README.

## Commands you will need

Run these **from `/home/sulthannauval/project/rantai/claw-ui`**.

| Purpose | Command | Expected on success |
|---|---|---|
| Typecheck | `bunx tsc --noEmit` | exit 0 |
| Build | `bun run build` | exit 0 |
| Tests | `bunx vitest run` | all pass |

There is no eslint config in this repo; `bun run lint` contributes nothing.

## Scope

**In scope**: `src/components/console/console-shell.tsx`, `src/proxy.ts`,
`src/lib/api.ts`, `src/lib/request-origin.ts`, the remaining `src/components/ops/`
panels, `src/approval`-equivalent display surfaces, `vitest.config.mts`,
`package.json` (dev dependencies only), `README.md`.

**Out of scope**: `src/components/ops/channels-panel.tsx` and `src/hooks/use-async.ts`
— plan 137 owns them; consume what it did. Anything in the RantAIClaw repo — if a fix
appears to need a backend field, STOP and report.

## Git workflow

- Branch: `fix/console-hardening-and-tests` (in the claw-ui repo)
- Conventional commits, e.g. `fix(console): stop Shift+Tab changing the autonomy level`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Stand up the component test harness first

Add `jsdom` and `@testing-library/react` as dev dependencies and widen
`vitest.config.mts`'s `include` to `src/**/*.test.{ts,tsx}`.

Do this **first**, not last: every remaining step in this plan is a React handler or
effect, and without this there is no way to write a regression test for any of them.

Before writing component tests, harvest the cheap wins: extract the
`readStartedAt < autonomyWrittenAt` staleness rule and the error-status mapping into
pure functions in `src/lib/` and test them there. The repo already does this well —
`src/lib/console.test.ts` is the model.

**Verify**: `bunx vitest run` → passes, and a trivial `.test.tsx` is now collected.

### Step 2: Make the autonomy binding safe and reversible

- Require a non-conflicting chord, or gate the binding on the chat composer having
  focus. At minimum, never let it **reach** `off` without a confirmation step.
- Stop calling `preventDefault()` when the rung would not change, so reverse-tab
  navigation works again.
- Capture the previous rung before the optimistic set; restore it in `catch`.
- Stamp `autonomyWrittenAt` in `.then()` only, so a failed write does not discard a
  correcting read.
- On failure, re-read the config once rather than waiting for the 30-second poll.

**Verify**: `bunx vitest run` → all pass.

### Step 3: Show the approval boundary

Read `channels_config.approval_owners` and `channels_config.autonomous_tools` from the
`GET /config` the panel already fetches, and render them read-only in the Telegram
card — owners as chips, `autonomous_tools` as a prominent warning badge when true.

Read-only first. Editing is a larger change and is not needed to close the finding,
which is that the boundary is **invisible**.

**Verify**: `bun run build` → exit 0.

### Step 4: Add an expected-Host allowlist to the BFF

Add an env-configured allowlist (default `localhost`, `127.0.0.1`, `[::1]`) checked in
`src/proxy.ts` for every `/api/rc/*` request, mirroring the existing
`RANTAICLAW_UI_TRUST_PROXY` pattern.

Default it to loopback plus whatever `RANTAICLAW_UI_DEV_ORIGINS` already carries, so an
operator running on a LAN address is not locked out silently — a lockout that looks
like an outage is its own failure.

Cover it in `bff-confinement.test.ts`, which already tests this layer.

**Verify**: `bunx vitest run` → all pass.

### Step 5: Map errors by status

Add one helper in `src/lib/api.ts` mapping `ApiError.status` to an operator-facing
sentence — 401 → re-login, 502 → "gateway unreachable — it may be restarting", else
the gateway's `detail` — and route every catch through it.

The proxy layer already labels these three; the UI currently throws the labels away,
so a session expiry, a restarting gateway and a genuine 400 all render identically.

**Verify**: `bunx vitest run` → all pass, including the new mapping tests in
`api-error.test.ts`.

### Step 6: Fix the remaining control defects

- **Skills toggle**: set the busy flag around the write and toast from the response's
  `enabled`, not the requested value.
- **Cron "Run now"**: when a run is refused by the security policy, show the full
  reason and offer an explicit "Run with approval" confirm that re-issues with
  `approved=true`. Do not send the flag silently — it is a privileged path.
- **MCP add/remove and providers save**: branch on the response rather than asserting
  success, and on a partial providers failure say which half landed and refresh
  regardless.
- **Config dump label**: soften "secrets redacted" to name what is actually
  guaranteed, and mask `mcp_servers.*.env` **values** client-side behind a per-row
  reveal.

**Verify**: `bunx vitest run` → all pass.

### Step 7: Correct the README

Delete the `RANTAICLAW_UI_PASSWORD` sentence and the `bun run pair` step. Add
`RANTAICLAW_UI_SECRET` and `RANTAICLAW_UI_TRUST_PROXY` to the configuration table —
both are already documented correctly in `.env.example`. Point the Auth bullet at
`docs/auth.md`, which is accurate. Move channels out of "read-only views", since the
panel now connects a bot and edits its allowlist.

Sweep `docs/DESIGN.md` for the same nonexistent variable.

**Verify**: `grep -rn 'RANTAICLAW_UI_PASSWORD' README.md docs/` returns nothing.

### Step 8: Drive it

1. Focus a button and press Shift+Tab — confirm focus moves backwards and the rung
   does **not** change.
2. Force an autonomy write to fail — confirm the displayed rung reverts.
3. Confirm the owners and `autonomous_tools` badge render.
4. Request `/api/rc/config` with a spoofed `Host` header — confirm it is rejected.

Record each observation in the PR.

## Test plan

1. `staleness_rule_ignores_failed_writes` — pure, at the lib layer.
2. `autonomy_rollback_restores_the_previous_rung` — component test.
3. `backtab_from_a_button_does_not_change_the_rung` — component test.
4. `unknown_host_is_rejected_by_the_bff` — in `bff-confinement.test.ts`.
5. `error_status_maps_to_a_distinct_message` — 401 / 502 / 400.
6. `skills_toggle_is_disabled_while_in_flight`.
7. `cron_refusal_shows_the_full_reason`.
8. `config_dump_masks_mcp_env_values`.

**Mutation check (required).** For test 3, restore the ungated binding and confirm it
**fails**. For test 4, remove the Host allowlist and confirm it **fails**. Restore both.

**Verify**: `bunx vitest run` → all pass.

## Done criteria

- [ ] `bunx tsc --noEmit` exits 0
- [ ] `bun run build` exits 0
- [ ] `bunx vitest run` passes, including the eight new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `vitest.config.mts` collects `.test.tsx` and jsdom is installed
- [ ] `grep -rn 'approval_owners' src/` returns at least one hit
- [ ] `grep -rn 'RANTAICLAW_UI_PASSWORD' README.md docs/` returns nothing
- [ ] The four drive observations from step 8 are in the PR body
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 138 updated (in the RantAIClaw repo)

## STOP conditions

Stop and report back if:

- Plan 137 has not merged — this is serialized over the same hook and panels.
- Adding jsdom or testing-library conflicts with the Next.js version pinned here.
  Report rather than downgrading anything.
- The Host allowlist locks out a legitimate deployment shape you cannot enumerate.
  Default it wider and document, rather than shipping a lockout.
- A fix appears to require a new backend field. That is a RantAIClaw change and this
  plan may not make it.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 137 fixed the channels panel and the shared async
  hook; this plan generalises the same shapes to the other panels. RantAIClaw plan 122
  changes what `approval_owners` means — if it lands first, step 3 should render
  whatever it settles on rather than today's semantics.
- **What a reviewer should scrutinise**: that step 1 genuinely landed before the
  component tests were written (a plan that adds the harness last usually ships without
  it), and that step 4's default cannot silently lock an operator out.
- **Deliberately deferred**: making the approval-boundary display **editable**. Showing
  it closes the finding; editing it is a new surface with its own authorization
  questions.
