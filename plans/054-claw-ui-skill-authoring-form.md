# Plan 054: Web console form to write and edit your own skills

> **Executor instructions**: This plan is executed in the **claw-ui repo**
> (`project/rantai/claw-ui`), not RantAIClaw. Follow it step by step and run
> every verification command. If anything in "STOP conditions" occurs, stop and
> report. When done, update the status row for this plan in
> `plans/README.md` (in RantAIClaw).
>
> **One rule governs the whole editor: the raw `SKILL.md` text is the only
> state.** The Form view is a projection of that text, not a second copy of
> the data. If you find yourself holding form fields and markdown as two
> pieces of state and syncing them, stop — that is the bug this design exists
> to prevent.
>
> **Drift check (run first)**: confirm `src/components/ops/skills-panel.tsx`
> still opens with `const installed = useAsync(() => api.skills(), [])` and
> renders a `Segmented` with `installed`/`browse`.

## Status

- **Priority**: P1
- **Effort**: M–L
- **Risk**: LOW (claw-ui only; no RantaiClaw behaviour changes)
- **Depends on**: 053 (needs the three content routes and `origin` on the list)
- **Category**: feature
- **Planned at**: RantAIClaw commit `6004757`, 2026-07-31

## Why this matters

Skills can be written from chat (`author_skill`) but nowhere else. A user who
wants to sit down and write one — or fix a typo in one they wrote last week —
has no surface for it. The console shows skills; it cannot author them.

## Design

### Single source of truth

Editor state is one string: the full `SKILL.md` text.

- **Markdown view** renders that string directly.
- **Form view** reads values out of it and writes edits back into their
  regions. Anything the form has no field for — a `## Troubleshooting` section
  the user typed by hand — is untouched and survives every round trip.

Switching views is a change of presentation, not a conversion. There is no
transform to lose data in, which is why the form/markdown sync bug class
cannot occur.

If the text no longer matches the expected shape (the user restructured it in
Markdown view, so the `## Instructions` list is gone), the **Form tab is
disabled entirely** with a short note, and editing continues in Markdown. One
rule, not a per-field degradation matrix.

### Form fields

Four, and only four — every one is something the loader actually reads:

| Field | Lives in | Read by |
|---|---|---|
| Nama | frontmatter `name:` | `load_skill_md` → `Skill.name` |
| Deskripsi | frontmatter `description:` | `Skill.description`; shown to the model |
| Tag | frontmatter `tags: [...]` | `Skill.tags` |
| Instruksi | `## Instructions` list items | body → the prompt |

**No "Tools" field.** `load_skill_md` sets `tools: Vec::new()` — nothing ever
parses a tool list out of `SKILL.md`. A structured-looking Tools field would
tell the user they are configuring something when they are only typing prose.
Tools belong in the instructions, as sentences.

### Where it lives

A modal, reusing the existing `Modal` component. The panel's two `Segmented`
views are lists you browse; authoring is a task you enter and leave, which is
what a modal expresses. It also keeps `skills-panel.tsx` from growing an
inline mode.

## Steps

### Step 1 — `src/lib/skill-md.ts`

Pure functions, no React, no fetch. This is where all the markdown handling
lives so it can be tested without rendering anything.

```ts
export interface SkillFields {
  name: string;
  description: string;
  tags: string[];
  instructions: string[];
}

// null when the text does not match the expected shape → Form tab disabled
export function readFields(md: string): SkillFields | null;

// returns a new md with only the named region replaced
export function writeField<K extends keyof SkillFields>(
  md: string, key: K, value: SkillFields[K]
): string;

export function emptyTemplate(): string;   // starting text for a new skill
export function slugify(name: string): string;  // preview only — server decides
```

`readFields` returns `null` if frontmatter is absent or unparseable, or if
there is no `## Instructions` list. `writeField` must never rewrite the whole
document — it replaces one region and leaves every byte outside it alone.

`slugify` here is a **preview**, mirroring the Rust rule (lowercase,
non-alphanumeric runs → single `-`, trimmed, capped at 64). The server's slug
is authoritative; never send the client-derived one as if it were.

**Verify**: vitest covering — round-trip with an unknown extra section
preserves it byte-for-byte; editing one field leaves the others untouched;
missing `## Instructions` returns `null`; a tag containing `,` or `]` does not
break out of the list; multi-line description collapses to one line (the
loader is line-based).

### Step 2 — API client and types

In `src/lib/api.ts`, beside the existing skills calls. `rc<T>(path, init?)`
(`api.ts:50`) is a thin `fetch` wrapper taking a normal `RequestInit`, so
method and body are passed the usual way, and it throws `ApiError` carrying
`.status` — which is how 409 and 413 are told apart from a generic failure.

```ts
skillContent: (slug: string) =>
  rc<{ slug: string; name: string; content: string }>(
    `skills/${encodeURIComponent(slug)}/content`),
saveSkillContent: (slug: string, content: string) => rc<...>(..., { method: "PUT", ... }),
createSkill: (name: string, content: string) => rc<...>("skills", { method: "POST", ... }),
```

**Address skills by `slug`, never by `name`.** Plan 053 switches the API to
slug addressing because `validate_slug` rejects the spaces a display name can
contain — a skill called `Kopi Pagi` lives in `kopi-pagi/` and is only
reachable at that address. `createSkill` is the one exception: it takes the
display name, because the slug does not exist yet and the server derives it.

In `src/lib/types.ts`, add to `Skill`:

```ts
slug?: string;
origin?: { kind: "authored" | "clawhub" | "bundled" | "git" | "local" };
```

Both optional, like the existing `clawhub` field. **Absent `origin` means the
skill is not editable** — the server resolves origin, including its
shape-based fallback for skills written before markers existed, so the client
never infers anything itself. Only `kind === "authored"` enables editing.

A card with no `slug` cannot be addressed and must not offer Edit, Disable, or
Uninstall — the only skills in that state are open-skills entries, which have
no directory of their own.

**Verify**: `npx tsc --noEmit` clean.

### Step 3 — `src/components/ops/skill-editor.tsx`

New file. Props: `mode: "create" | "edit"`, `slug?: string`, `onClose`,
`onSaved`.

State: `md: string`, `view: "form" | "markdown"`, `saving`, `error`.
**`md` is the only data state.** Form inputs read through `readFields(md)` and
write through `writeField`.

On open in edit mode, fetch via `api.skillContent(slug)`. On create, start from
`emptyTemplate()`.

Four things the UI must get right:

1. **Slug preview** under the Nama field (`Folder: kopi-pagi`) so the user sees
   the directory name before saving.
2. **Collision, checked client-side on both keys** against the already-loaded
   skills list — name *and* derived slug, since two different names can
   slugify to the same directory (`Kopi Pagi` and `kopi  pagi` both give
   `kopi-pagi`). Disable Save and say which existing skill it would clash
   with. The server returns `409` for both; the client check is for latency,
   not authority, so handle the `409` too.
3. **Renaming in edit mode is refused, before sending.** Plan 053's `PUT`
   answers 400 if the submitted `name:` no longer matches the skill at that
   slug — the directory would keep the old slug while the manifest claimed a
   new name. In edit mode the Nama field must therefore be read-only, with a
   note that renaming means creating a new skill. Do not silently discard a
   change the user typed.
4. **Size guard.** The gateway caps request bodies at 64 KiB and that includes
   JSON escaping, which inflates newline-dense markdown. Check
   `new Blob([JSON.stringify({content: md})]).size` before sending and refuse
   with a clear message. A bare `413` is unreadable to the user.

**Verify**: `npx next build` clean.

### Step 4 — wire into `skills-panel.tsx`

Two additions, nothing else in this file:

- A `+ Tulis` button beside `RefreshButton`, shown only on the `installed`
  view, opening the editor in create mode.
- A pencil `IconButton` on each card, rendered **only** when
  `s.origin?.kind === "authored" && s.slug`, opening the editor in edit mode
  keyed on `s.slug`.

The existing `toggle` and `uninstall` handlers currently pass `s.name`; switch
both to `s.slug` to match plan 053's addressing. This is the two-line change
that makes Disable and Uninstall work for skills with a spaced display name —
without it the console can create a skill it cannot manage.

On save, call `installed.refresh()` — the same pattern `toggle`/`install`/
`uninstall` already use.

Do not restyle, reorder, or refactor the existing card layout.

**Verify**: `npx next build` clean; pencil appears on an authored skill and on
no other; Disable and Uninstall work on a skill whose display name has a
space.

## STOP conditions

- Form fields held as state separately from `md`.
- `writeField` regenerating the whole document rather than patching a region.
- A Tools field appearing in the form.
- The pencil icon rendering for any origin other than `authored`, for a skill
  with no `origin`, or for a skill with no `slug`.
- Addressing any route by `name` instead of `slug` (except `createSkill`,
  where the slug does not exist yet).
- An editable Nama field in edit mode.
- Sending the client-derived slug to the server as authoritative.
- Editing files inside a skill directory other than `SKILL.md`.
- Changes to `skills-panel.tsx` beyond the two additions in step 4.

## Verification

```bash
npx tsc --noEmit
npx vitest run src/lib/skill-md.test.ts
npx next build
```

Note on linting: `package.json` defines `"lint": "next lint"`, but the repo
ships **no eslint config**, so the script would try to scaffold one
interactively rather than check anything. Do not run it and do not add a
config — that is a separate decision, not part of this plan.

Manual, against a running gateway with plan 053 merged:

1. `+ Tulis` → fill the four fields with a display name containing a space
   (`Kopi Pagi`) → Simpan → skill appears with a `buatan sendiri` badge and a
   pencil, listed as `Kopi Pagi`, directory `kopi-pagi`.
2. On that same card, use Disable and then Enable. Both must succeed — a
   spaced display name returns 400 before plan 053, so this is the end-to-end
   check that the addressing fix reached the console.
3. Reopen it → Markdown view → add a `## Troubleshooting` section → back to
   Form → the four fields are still correct → Simpan → confirm on disk that
   the new section is present.
4. Open a bundled skill (`summarizer`) → no pencil. Confirm on disk it is
   byte-identical afterwards.
5. Try to create a skill named after an existing one → Save disabled with an
   explanation. Repeat with a *different* name that slugifies to an existing
   directory — also refused.
6. In Markdown view, delete the `## Instructions` heading → Form tab disables
   with a note, Markdown still saves.
7. In edit mode, confirm the Nama field is read-only.

## Rollback

Revert the commit. The panel returns to list-only; skills created through the
editor stay on disk as ordinary authored skills, editable by hand and via the
routes from plan 053.
