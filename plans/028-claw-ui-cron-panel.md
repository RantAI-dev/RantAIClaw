# Plan 028: claw-ui Schedules panel — wire, edit, run-history, presets

> **REPO**: `claw-ui` (separate Next.js repo at `../claw-ui`, NOT this repo).
> Kept here so the whole cron effort's plans live together.
>
> **Context**: The Schedules panel (`src/components/ops/cron-panel.tsx`) is a
> create + list + toggle/run/delete surface that is (a) fully broken until plan
> 027 ships the backend endpoints, and (b) thin even once wired: no edit form, no
> polling, no run history, no timezone control, no cron validation, agent-jobs-only,
> ephemeral run output. This plan wires it to the real API (027) and fills those
> gaps.
>
> **Depends on**: plan 027 (the `/api/v1/cron*` endpoints must exist). Verify a
> running gateway serves them before manual testing.
> **Executor note**: claw-ui uses `bun`/`npm`. Scripts (`package.json`):
> `build` = `next build`, `test` = `vitest run`. **Verify with
> `bun run build && bun run test`** (or `npm run …`). Do NOT use `bun run lint` —
> `next lint` was REMOVED in Next 16 (`next` is `16.0.10`; no eslint dep/config),
> so `bun run lint` fails with a spurious "no such directory: …/lint" error
> unrelated to your code. This is a pre-existing repo breakage, not yours. `next
> build` typechecks (`next.config.mjs` sets `typescript.ignoreBuildErrors: false`),
> so build is the real gate. Next 16 + React 19. No automated E2E harness — pure
> logic is vitest-tested; UI is verified by driving `bun run dev` (:3939) against a
> live gateway (027 merged).
> **Branch**: `feat/cron-panel` (in the claw-ui repo). **Risk**: MEDIUM (frontend
> only; no exposure boundary).

## Baseline evidence (claw-ui, confirmed 2026-07-18)

- Panel: `src/components/ops/cron-panel.tsx` (one-shot `useAsync(() => api.cron(), [])`
  at line 61; destructures `{ data, loading, error, refresh }` — **drops the
  `refreshing` flag** that `src/hooks/use-async.ts:37` exposes, so manual refresh
  has no visual feedback). Local `describeCron` (lines 34-58) + `fmtWhen` (19-28).
- API client: `src/lib/api.ts:116-128` — `cron`, `createCron` (agent-only body),
  `updateCron` (full body defined but only ever called with `{enabled}` at
  `cron-panel.tsx:117`), `deleteCron`, `runCron`. No `runsCron`.
- Types: `src/lib/types.ts:89-92` (`CronSchedule` union) + `:270-279` (`CronJob`,
  missing `schedule`/`prompt`/`command`/`session_target`/`model`/`last_output`/
  `created_at`/`delete_after_run`).
- Proxy: `src/app/api/rc/[...path]/route.ts` relays `GET/POST/PUT/DELETE` (and
  query strings) → `<gateway>/api/v1/<path>`. So `runsCron` and `?approved=` work
  through it unchanged.
- Nav/entry: route id `cron`, label "Schedules" (`src/lib/console.ts:34,55,101`),
  rendered via `PANELS.cron = <CronPanel />` (`src/components/console/ops-view.tsx:26,55`).

**API contract consumed (from plan 027):**
`GET /cron → {jobs:CronJob[],count}`, `POST /cron` (agent+shell),
`PUT /cron/{id}`, `DELETE /cron/{id} → {id,deleted}`,
`POST /cron/{id}/run?approved= → {id,success,output}`,
`GET /cron/{id}/runs?limit= → {runs:CronRun[],count}`. `CronJob` now carries the
full backend fields.

---

## Stage C — Wire + edit + shell/agent (make it correct & complete)

### Task 1 — Types: extend `CronJob`, add `CronRun`

**Files:** `src/lib/types.ts`.

- [ ] Replace the `CronJob` interface (`:270-279`) and add `CronRun`:

```ts
export interface CronJob {
  id: string;
  name: string | null;
  expression: string;
  schedule: CronSchedule;
  job_type: string; // "agent" | "shell"
  command: string;
  prompt: string | null;
  session_target: string; // "isolated" | "main"
  model: string | null;
  enabled: boolean;
  delete_after_run: boolean;
  created_at: string;
  delivery: { mode: string; channel: string | null; to: string | null; best_effort: boolean };
  next_run: string | number | null;
  last_run: string | number | null;
  last_status: string | null;
  last_output: string | null;
}

export interface CronRun {
  id: number;
  job_id: string;
  started_at: string;
  finished_at: string;
  status: string; // "ok" | "error"
  output: string | null;
  duration_ms: number | null;
}
```

- [ ] **Verify:** `bun run build` typechecks (existing `cron-panel.tsx` references
  only the retained fields — no break).

### Task 2 — API client: run-history + shell-capable create/run

**Files:** `src/lib/api.ts`.

- [ ] Update the cron block (`:116-128`) to:

```ts
  cron: () => rc<{ jobs: CronJob[]; count: number }>("cron"),
  createCron: (body: {
    schedule: CronSchedule;
    job_type?: "agent" | "shell";
    prompt?: string;
    command?: string;
    name?: string;
    model?: string;
    session_target?: "isolated" | "main";
    delete_after_run?: boolean;
  }) => rc<CronJob>("cron", { method: "POST", body: JSON.stringify(body) }),
  updateCron: (
    id: string,
    body: {
      enabled?: boolean;
      name?: string;
      prompt?: string;
      command?: string;
      model?: string;
      schedule?: CronSchedule;
      session_target?: "isolated" | "main";
      delete_after_run?: boolean;
    },
  ) => rc<CronJob>(`cron/${encodeURIComponent(id)}`, { method: "PUT", body: JSON.stringify(body) }),
  deleteCron: (id: string) =>
    rc<{ id: string; deleted: boolean }>(`cron/${encodeURIComponent(id)}`, { method: "DELETE" }),
  runCron: (id: string, approved = false) =>
    rc<{ id: string; success: boolean; output: string }>(
      `cron/${encodeURIComponent(id)}/run${approved ? "?approved=true" : ""}`,
      { method: "POST" },
    ),
  cronRuns: (id: string, limit = 50) =>
    rc<{ runs: CronRun[]; count: number }>(
      `cron/${encodeURIComponent(id)}/runs?limit=${limit}`,
    ),
```

- [ ] Add `CronRun` to the type imports at the top of `api.ts` (`:2-28`).
- [ ] **Verify:** `bun run build` typechecks.

### Task 3 — Panel hygiene: refresh feedback, full form reset, richer rows

**Files:** `src/components/ops/cron-panel.tsx` (+ maybe `src/components/ops/shared.tsx`
`RefreshButton`).

- [ ] Destructure `refreshing` from `useAsync` (`cron-panel.tsx:61`) and pass it to
  `RefreshButton` so it spins/disables during a manual refresh:

```tsx
  const { data, loading, error, refresh, refreshing } = useAsync(() => api.cron(), []);
  ...
  <SectionTitle action={<RefreshButton onClick={refresh} spinning={refreshing} />}>
```

  If `RefreshButton` (`shared.tsx`) has no `spinning`/`busy` prop, add an optional
  one that applies `animate-spin` to the icon and `disabled` to the button. Keep
  it backward-compatible (optional prop).

- [ ] Declare the `command` state now (Task 4 adds the shell UI that binds it;
  declaring it here keeps this task's build green):
  `const [command, setCommand] = React.useState("");`
- [ ] On create success (`cron-panel.tsx:103-107`), reset ALL form fields (today
  `expr`/`kind`/`everyMin`/`at` persist):

```tsx
      toast.success("Cron job created");
      setPrompt("");
      setName("");
      setModel("");
      setCommand("");       // command state declared above (used by Task 4)
      setAt("");
      // leave `expr`/`kind`/`everyMin` at sensible defaults or reset as desired
      refresh();
```

- [ ] Show the job's prompt/command + last-run in the list row
  (`cron-panel.tsx:252-261`) so an unnamed job is identifiable:

```tsx
                <div className="truncate font-mono text-[11px] text-muted-foreground">
                  {j.expression || j.schedule.kind} · next {fmtWhen(j.next_run)}
                  {j.last_status ? ` · last: ${j.last_status} (${fmtWhen(j.last_run)})` : ""}
                </div>
                {(j.prompt || j.command) && (
                  <div className="truncate text-[11px] text-muted-foreground/80">
                    {j.job_type === "shell" ? j.command : j.prompt}
                  </div>
                )}
```

- [ ] **Verify:** `bun run build`; drive `bun run dev` — refresh button spins;
  list shows prompt/command + last-run.

### Task 4 — Shell/agent job creation

**Files:** `src/components/ops/cron-panel.tsx`.

- [ ] Add state + a job-kind toggle to the create form. Add `command` state and a
  `jobKind` selector (`"agent" | "shell"`); render a prompt `Textarea` for agent
  or a command `Input` for shell; send the matching field:

```tsx
  const [jobKind, setJobKind] = React.useState<"agent" | "shell">("agent");
  // `command` state was declared in Task 3 — do not redeclare it here.
  ...
  // in create(), after building `schedule`:
    const payload =
      jobKind === "shell"
        ? { schedule, job_type: "shell" as const, command: command.trim(),
            name: name.trim() || undefined }
        : { schedule, job_type: "agent" as const, prompt: prompt.trim(),
            name: name.trim() || undefined, model: model.trim() || undefined };
    if (jobKind === "shell" ? !command.trim() : !prompt.trim()) return;
    setBusy(true);
    try {
      await api.createCron(payload);
      ...
```

  Update the create button `disabled` guard to check the active field
  (`!prompt.trim()` → `jobKind === "shell" ? !command.trim() : !prompt.trim()`).
  Update the "New agent job" header (`:159`) to reflect the selected kind.

- [ ] **Verify:** create both an agent job and a shell job against a live gateway;
  confirm they appear with the correct `job_type` badge. A disallowed shell
  command surfaces the 027 security-policy 400 as a toast.

### Task 5 — Edit an existing job

**Files:** `src/components/ops/cron-panel.tsx` (add an edit modal/inline form).

- [ ] Add an Edit (pencil) `IconButton` per row (`cron-panel.tsx:263-281`) that
  opens a form pre-filled from the job. On save, call
  `api.updateCron(id, { name, prompt|command, model, schedule })` (only changed
  fields), then `refresh()`. Reuse the schedule inputs from the create form. This
  is the first real caller of `updateCron`'s full body.

  Keep it simple: reuse the existing generic `Modal`
  (`src/components/ui/modal.tsx` — title/description/footer/children, Esc +
  backdrop close, the same primitive `ConfirmModal` is built on), or an inline
  expandable row. Do NOT build a separate route.

- [ ] **Verify:** edit a job's name/prompt and schedule; confirm the PUT persists
  and the list reflects it after refresh.

- [ ] **Commit Stage C** (in claw-ui): one commit per task, e.g.
  `feat(cron): wire Schedules panel to the live API — shell/agent create + edit`.

---

## Stage D — Depth: polling, run history, timezone, presets/validation

### Task 6 — Live refresh (polling)

**Files:** `src/components/ops/cron-panel.tsx`.

- [ ] Add a light interval poll so `next_run`/`last_status` update without a
  manual refresh (a job firing in the background should surface):

```tsx
  React.useEffect(() => {
    const t = setInterval(refresh, 15000);
    return () => clearInterval(t);
  }, [refresh]);
```

  `useAsync` keeps stale content mounted during a refresh (`refreshing`), so this
  won't flash. (Optional: pause polling when `document.hidden`.)

- [ ] **Verify:** create a `*/1 * * * *` job on a live gateway; within a poll cycle
  the row's `last_status`/`last_run` update on their own.

### Task 7 — Run history view

**Files:** `src/components/ops/cron-panel.tsx` (+ a small `CronRunsModal`).

- [ ] Add a "history" (clock) `IconButton` per row that opens a modal (build on
  the existing generic `Modal`, `src/components/ui/modal.tsx`) listing
  `api.cronRuns(id)` results: each run's `started_at`, `status`, `duration_ms`,
  and a collapsible `output`. This replaces the ephemeral 200-char run toast as
  the durable record.

- [ ] **Verify:** run a job a few times (Run-now button), open history, confirm
  past runs + outputs are listed (backed by 027's `GET /cron/{id}/runs`).

### Task 8 — Timezone for cron schedules

**Files:** `src/components/ops/cron-panel.tsx`.

- [ ] For `kind === "cron"`, add an optional IANA timezone `Input` (or a small
  select of common zones) that sets `tz` on the `CronSchedule`:
  `{ kind: "cron", expr, tz: tz.trim() || undefined }`. The backend already
  supports `tz` (`Schedule::Cron.tz`, `src/cron/schedule.rs:14-21`). Update the
  preview footnote (`:235,286`) to show the chosen zone instead of the misleading
  "server time zone" when a tz is set.

- [ ] **Verify:** create a cron job with `tz = "America/Los_Angeles"`; confirm the
  returned job's `next_run` reflects the zone conversion.

### Task 9 — Cron presets + client-side validation (pure logic + vitest)

**Files:** Create `src/lib/cron.ts` + `src/lib/cron.test.ts`; use them in the panel.

- [ ] Move `describeCron` out of the panel into `src/lib/cron.ts` and add presets +
  a validator:

```ts
export const CRON_PRESETS: { label: string; expr: string }[] = [
  { label: "Every hour", expr: "0 * * * *" },
  { label: "Every day at 9:00", expr: "0 9 * * *" },
  { label: "Weekdays at 9:00", expr: "0 9 * * 1-5" },
  { label: "Every Monday 9:00", expr: "0 9 * * 1" },
  { label: "1st of month 00:00", expr: "0 0 1 * *" },
  { label: "Every 15 minutes", expr: "*/15 * * * *" },
];

/** Validate a 5-field cron expression. Returns null if valid, else a message. */
export function validateCron(expr: string): string | null {
  const f = expr.trim().split(/\s+/);
  if (f.length !== 5) return "Cron needs 5 fields: min hour day month weekday";
  const ranges: [number, number][] = [[0, 59], [0, 23], [1, 31], [1, 12], [0, 7]];
  for (let i = 0; i < 5; i++) {
    if (!validField(f[i], ranges[i][0], ranges[i][1]))
      return `Field ${i + 1} ("${f[i]}") is out of range`;
  }
  return null;
}

function validField(field: string, min: number, max: number): boolean {
  if (field === "*") return true;
  return field.split(",").every((part) => {
    const [range, step] = part.split("/");
    if (step !== undefined && !/^\d+$/.test(step)) return false;
    if (range === "*") return true;
    const [a, b] = range.split("-");
    if (!/^\d+$/.test(a) || Number(a) < min || Number(a) > max) return false;
    if (b !== undefined && (!/^\d+$/.test(b) || Number(b) < Number(a) || Number(b) > max))
      return false;
    return true;
  });
}
```

  (Move the existing `describeCron` + `DOW` into this file and export them.)

- [ ] Add `src/lib/cron.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { validateCron } from "./cron";

describe("validateCron", () => {
  it("accepts valid expressions", () => {
    for (const e of ["0 9 * * *", "*/15 * * * *", "0 9 * * 1-5", "0 0 1 * *"])
      expect(validateCron(e)).toBeNull();
  });
  it("rejects wrong field count", () => {
    expect(validateCron("0 9 * *")).toMatch(/5 fields/);
  });
  it("rejects out-of-range fields", () => {
    expect(validateCron("99 9 * * *")).toMatch(/out of range/);
    expect(validateCron("0 25 * * *")).toMatch(/out of range/);
  });
});
```

- [ ] In the panel: render `CRON_PRESETS` as quick-fill buttons (set `expr`), and
  show `validateCron(expr)` inline; disable Create when the cron expression is
  invalid (in addition to the existing empty/required guards). Replace the local
  `describeCron` import.

- [ ] **Verify:** `bun run test` (vitest) green; `bun run build` clean; drive the
  panel — presets fill the field, an invalid expression blocks Create with an
  inline message.

- [ ] **Commit Stage D**: one commit per task, e.g.
  `feat(cron): run history, polling, timezone, and cron presets/validation`.

---

## Done criteria (all must hold)
- [ ] Panel works end-to-end against a live gateway (027 merged): list, create
  (agent AND shell), edit, enable/disable, run-now, delete.
- [ ] Manual refresh shows feedback; the panel auto-polls (15s).
- [ ] Run history is viewable per job; timezone can be set on cron jobs.
- [ ] Cron expressions are validated client-side with presets; `validateCron` unit
  tests pass.
- [ ] `bun run build && bun run test` all clean (no `bun run lint` — `next lint`
  was removed in Next 16; build is the typecheck gate).
- [ ] `CronJob`/`CronRun` types match 027's response shapes.

## STOP conditions
- If 027 is not deployed to the gateway the UI talks to, STOP manual testing —
  every call 404s (the pre-plan state). Type/vitest/build checks still run.
- If `RefreshButton` (`shared.tsx`) can't take a spinning prop without a wider
  refactor, keep the destructure fix and skip the visual spin (note it) rather
  than restructuring shared components.
- Do NOT expose `session_target` (Main/Isolated) as a functional control — it is
  inert in the engine (plan 026 B5, out of scope). If shown at all, label it
  "reserved".

## Rollback
Frontend-only; revert the branch. No persisted state. Reverting leaves the
pre-plan panel (which is inert without 027 anyway).

## Two-repo release note
Per the release memo: a claw-ui feature that pairs with a backend feature ships
by cutting claw-ui first, then bumping the claw-ui pin in `webui.rs` on the
RantAIClaw side. Coordinate 027 (backend) + 028 (frontend) versions at release.
