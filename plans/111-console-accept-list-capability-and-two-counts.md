# Plan 111: claw-ui: the accept list, the capability signal, and the two document counts

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- (claw-ui repo) src/lib/attachments.ts src/components/ops/ · src/kb/file/mod.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P3
- **Effort**: S
- **Risk**: LOW
- **Depends on**: 095, 097, 100 (each supplies a value this plan renders)
- **Category**: bug
- **Planned at**: commit `2ca7e59`, 2026-08-10

> **Repo note**: mostly the separate `claw-ui` repository at `../claw-ui`.
> One item (Step 1) may touch the Rust side; keep them in step.

## Why this matters

Four small console defects that each make the KB look broken or lie about it.

**1. The file picker offers formats the backend refuses.**
`attachments.ts:7-35` lists `.docx`, `.xlsx` and `.css`.

- `.docx` / `.xlsx` are detected as `SupportedFileType::Document`
  (`src/kb/file/mod.rs:89`) but processing requires the `kb-office` feature,
  which is **not** in `default` (`Cargo.toml:246`). On a stock build the upload
  transfers fully and then fails with "office files require the kb-office
  feature" (`file/mod.rs:182-185`).
- `.css` is in neither `TEXT_EXTENSIONS` nor any other list
  (`file/mod.rs:98-102`), so `detect_file_type` returns `None` and the ingest
  route answers 400 `unsupported_file_type`.

The backend even has `supported_extensions()` (`file/mod.rs:136-150`) whose doc
comment says it is "useful for callers wiring up … UI dropdowns" — the console
lives in another repo and does not use it, so the two lists drifted. The
irony is that `file/mod.rs:84-88` warns against exactly this class of mismatch.

**2. The re-extract toast contradicts the tab beside it.**
`doc-intelligence-drawer.tsx:60-63` prints `r.entities`, which counts raw
extractions before global dedup (plan 095). The Entities tab under it lists the
stored set.

**3. The drawer ignores `capability`.** The type carries it
(`types.ts:242`) and the empty state (`:93-104`) hard-codes "Extraction may not
have run yet … set `KB_INTELLIGENCE_ENABLED`" — an instruction with no UI path,
and wrong when the real cause is a missing credential.

**4. One knowledge base, two document counts.** The card list uses
`group.document_count` (unfiltered, `groups.rs:76`); the detail view prefers
`docs.data?.length` (filtered) — `kb-panel.tsx:656`. After a soft delete the
card says 5 and the detail says 4. Plan 100 fixes the backend; this plan makes
the console stop papering over it.

## Current state (verified at 2ca7e59)

- `ACCEPT_EXTS` / `IMAGE_EXTS` — `attachments.ts:7-35`, `:47`
- Backend lists — `file/mod.rs:81-102`; `IMAGE_EXTENSIONS` also has `.heic`,
  which the console omits (harmless direction)
- `attachments.ts:45-46` claims images need "no vision chat model" — wrong:
  `process_image` posts to `extract_vision_base_url`, a chat-completions
  endpoint, and requires a credential

## Scope

**In scope**: the four items above.

**Out of scope**: sharing the extension list across repos automatically. Worth
doing one day; a comment pointing at the source of truth is enough here.

## Git workflow

```bash
cd ../claw-ui && git switch -c fix/kb-console-accuracy
```

## Steps

### Step 1: Align the accept list

Remove `.css`. For `.docx`/`.xlsx`, pick one and say which in the PR:

- **(a)** drop them from `ACCEPT_EXTS` — matches a stock build, no surprise
  failure; or
- **(b)** add `kb-office` to the default features in `Cargo.toml:246` and keep
  them — a real capability gain, but it pulls `docx-rs` + `calamine` into every
  build and binary size is a stated product goal (CLAUDE.md §2.3).

Recommend **(a)**. Add a comment naming `src/kb/file/mod.rs:81-102` as the
source of truth so the next editor knows where to look.

Also fix the wrong comment at `attachments.ts:45-46`, and add `.heic` to
`IMAGE_EXTS` to match the backend.

**Verify**: every extension in `ACCEPT_EXTS` and `IMAGE_EXTS` appears in the
backend lists. Do the comparison explicitly, do not eyeball it.

### Step 2: Truthful re-extract toast

Depends on plan 095 returning a deduplicated count. Reword to name the scope:

```tsx
        `Found ${formatNumber(r.entities)} entities · ${formatNumber(r.relations)} relations in this document`,
```

If 095 has not landed, do not reword — a nicer sentence around a wrong number
is worse. Note the dependency and skip.

### Step 3: Use `capability` in the drawer

Read `intel.data?.capability` and branch the empty state the same way
`graph-lens` does after plan 097: disabled / no-credential / genuinely empty.
Reuse `deriveGraphState` rather than writing a second copy of the logic — the
two surfaces disagreeing is how this started.

### Step 4: One count, one source

With plan 100 landed, `group.document_count` is correct, so make it the
**preferred** source in both views rather than the last resort. Today
`kb-panel.tsx:656` prefers the locally-fetched list length, which quietly hides
a server-side divergence instead of revealing it. `document_count` is optional
in the type (`types.ts:174`), so the fallback chain stays — only the order
changes:

```tsx
  const docCount = group.document_count ?? docs.data?.length ?? 0;
```

**Verify**: soft-delete a document; card and detail agree without a refresh
of the other view.

### Step 5: Test the pure logic

Extend the `graph-lens-helpers` test file from plan 097 to cover the drawer's
use of the same function. Keep it a pure-function test — no component rendering
needed for the thing that was wrong.

## Test plan

```bash
cd ../claw-ui
npx vitest run
npx next build
npx next start -p 3939
```

Drive it:

- the file picker no longer offers a format the backend rejects — try one of
  each removed extension and confirm it is not selectable
- re-extract a document; toast count matches the Entities tab
- with extraction on and no key, the drawer names the missing credential
- soft-delete a document; both counts agree

## Done criteria

- No extension is offered that the backend cannot process.
- Toast, tab and graph agree on entity counts.
- The drawer explains an empty state instead of guessing.
- One document count, one source.

## STOP conditions

- Option (b) is chosen in Step 1: measure the binary-size delta and put it in
  the PR body. A default-feature addition needs that number.
- Plans 095/097/100 have not landed — Steps 2-4 render values that are still
  wrong. Do Step 1 alone and say so.
