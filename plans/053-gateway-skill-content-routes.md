# Plan 053: Gateway routes to read, write, and create a skill's `SKILL.md`

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **The console cannot reach the filesystem.** Every capability the editor
> needs must exist as a route here first. But only three routes are needed —
> if you find yourself adding a fourth, re-read "Scope".
>
> **Drift check (run first)**:
> `git diff --stat 6004757..HEAD -- src/gateway/api_v1.rs src/gateway/mod.rs src/skills/mod.rs`
> Compare the "Current state" excerpts against live code.

## Status

- **Priority**: P1
- **Effort**: M–L
- **Risk**: MED-HIGH (`src/gateway/**` — new write surface on the API, plus a
  behaviour change to two shipped routes)
- **Depends on**: 052 (needs `Skill.origin`)
- **Blocks**: 054
- **Category**: feature
- **Planned at**: commit `6004757`, 2026-07-31

## Why this matters

`GET /api/v1/skills/{name}` returns parsed metadata only — name, description,
tags, tool names, enabled/active flags. **The `SKILL.md` body is never sent.**
An editor cannot load a skill it cannot read, and cannot save one it cannot
write.

The write side is a genuine exposure widening and must be treated as one. A
skill body is injected into the system prompt on every turn
(`load_skill_md` sets `prompts: vec![content]` — the *entire file*), so a route
that writes one is a route that rewrites the agent's standing instructions.
That is the same reasoning that put `author_skill` and `skills_install` in
`OWNER_ONLY_TOOLS` (`src/approval/guest.rs:52-56`, "a persistent
prompt-injection primitive").

Two properties follow, and both are load-bearing:

1. **Owner-scoped.** `check_auth` on every route, exactly as the existing
   skills routes do. No new auth mode, no new token.
2. **Authored-only.** Read and write are refused unless the skill's origin is
   `authored`. Not a courtesy — a route that can rewrite a bundled or
   ClawHub skill lets a caller replace vendor-reviewed content with arbitrary
   instructions while the console still shows the trusted badge.

## Scope

Three new routes, plus a correction to two existing ones that this feature
would otherwise break (see "Skills must be addressed by slug").

```
GET  /api/v1/skills/{slug}/content   read raw SKILL.md      (authored only)
PUT  /api/v1/skills/{slug}/content   overwrite raw SKILL.md (authored only)
POST /api/v1/skills                  create a new skill     (writes origin marker)
```

Explicitly **not** in scope: rename, duplicate/fork, delete (already exists as
`DELETE /api/v1/skills/{slug}`), listing raw bodies in bulk, editing anything
other than `SKILL.md` inside a skill directory.

## Skills must be addressed by slug, not by manifest name

**This is a confirmed live bug that this feature would make routine.**

A skill's manifest `name:` is free text. `load_skill_md` uses it verbatim
(`mod.rs:937`, `frontmatter_name.unwrap_or(dirname)`), so `Skill.name` can
contain spaces and capitals. `author_skill` writes exactly that — the display
name the user asked for — while the *directory* gets `slugify(name)`.

Reproduced on a live install:

```console
$ rantaiclaw skills list
  ✓ Name Space Probe v0.1.0 — Cek nama berspasi.
$ rantaiclaw skills disable "Name Space Probe"
  ✓ Name Space Probe disabled.          # CLI is fine
```

But `validate_slug` rejects spaces — asserted by its own test at
`clawhub.rs:1448`, `validate_slug("with space").is_err()` — and two routes call
it on the `{name}` path parameter: `skills_set_enabled` (`api_v1.rs:1157`) and
`skills_uninstall` (`api_v1.rs:1222`). Both answer **400** for that skill.
`skills_show` does not call it and works, so the API is inconsistent with
itself as well as with the CLI.

Today this is latent: ClawHub slugs are already slug-shaped. This feature ends
that — every skill created through the console gets a human display name, so
the console would be able to create and edit a skill it cannot disable or
delete. Shipping that is not acceptable, so the correction belongs here rather
than in a follow-up.

**The fix**: address skills over the API by **slug** — the directory name,
which is `[a-z0-9-]` by construction and already what `validate_slug` expects.

1. `skill_status_json` gains `"slug"`, derived from the skill's directory
   (`location.parent().file_name()`). `"name"` stays as the display name.
2. The new content routes take `{slug}` and resolve by directory, not by
   manifest name.
3. `skills_set_enabled` and `skills_uninstall` accept the slug. Their
   `validate_slug` guard stays exactly as it is and now guards the thing it
   was written for.

Backward compatibility: for every skill installed from ClawHub or a bundled
pack, slug and name are already identical, so existing clients keep working
unchanged. The skills whose behaviour changes are precisely the ones that
return 400 today.

`set_skill_enabled` writes `[skills.entries.<key>]` keyed by the resolved
skill's **name** — verified: disabling the probe above produced
`[skills.entries."Name Space Probe"]`. Do not change that key to the slug;
the config contract is already shipped and the resolver reads it by name.
Route in by slug, resolve to the skill, then let the existing writer use the
name as it does now.

## Current state

Routes today (`src/gateway/api_v1.rs:53-59`):

```rust
.route("/api/v1/skills", get(skills_list))
.route("/api/v1/skills/install", post(skills_install))
.route("/api/v1/skills/{name}", get(skills_show).delete(skills_uninstall))
.route("/api/v1/skills/{name}/enabled", put(skills_set_enabled))
```

Helpers already available and to be reused rather than re-implemented:
`check_auth`, `err_400`, `err_404`, `err_500`, `err_for_skill_lookup`,
`crate::skills::clawhub::validate_slug`, `load_skills_with_status`.

Two functions this plan needs are currently private and must be widened to
`pub(crate)` — no other change to either:

- `slugify` (`src/tools/author_skill.rs:67`) — derives the directory name.
  Its guarantee that output is only `[a-z0-9-]` is what makes traversal
  impossible by construction; do not write a second implementation.
- `parse_yaml_frontmatter` (`src/skills/mod.rs:1167`) — used to validate that
  a submitted body is loadable.

## Hard constraint: 64 KiB request bodies

`RequestBodyLimitLayer::new(MAX_BODY_SIZE)` (64 KiB, `src/gateway/mod.rs:49`)
applies to the `api_v1` routes — the comment at `mod.rs:833-838` says so
explicitly, and deliberately exempts only the KB router.

This is not theoretical. A real shipped skill (`RantAI-Copilot`'s
`hypervisor`) is **59,570 bytes**. JSON-encoding inflates that further: every
newline becomes `\n` (+1 byte each, roughly +850 for a file that size) plus
quote and backslash escapes. A file near 58 KiB can cross the cap once encoded.

**Do not raise the limit.** It guards the whole exposed API, and widening an
exposure boundary for one convenience case is exactly what CLAUDE.md §3.6
forbids. Instead:

- `GET` is unaffected — `RequestBodyLimitLayer` caps *requests*, so reading a
  large skill works at any size.
- `PUT`/`POST` are capped. 64 KiB is roughly 15,000 words, far beyond any
  hand-written skill, so the cap only bites for machine-generated or imported
  content.
- The failure must be legible. A bare 413 from the middleware tells the user
  nothing. Plan 054 checks the encoded size client-side and explains it before
  sending; this plan must confirm the server-side 413 is at least reached
  rather than surfacing as a dropped connection.

## Design

### Origin gate

One helper, used by both content routes:

```rust
fn require_authored(skill: &Skill) -> Result<&Path, (StatusCode, Json<ErrorBody>)>
```

Returns the skill's directory when `origin` is `Some(kind: Authored)`.
Otherwise `403` with a body naming the actual origin, so the console can say
something true (`"weather is managed by ClawHub and cannot be edited here"`).

`403`, not `404`: the skill exists and the caller is authorised to see it —
they are just not permitted to edit it. Hiding it would make the console's
own list inconsistent with its errors.

### Validation on write

Four checks, in this order, all before touching disk:

1. **Frontmatter parses** and yields a non-empty `name`. A body the loader
   cannot read would install a skill that silently never appears.
2. **`name` is unchanged.** On `PUT`, the submitted body's `name:` must equal
   the current skill's name **exactly** — byte-for-byte, not
   case-insensitively and not "slugifies to the same directory".

   The stricter rule is the correct one because the name is a key in two
   places the slug is not: `[skills.entries.<name>]` in config (verified —
   disabling a probe skill produced `[skills.entries."Name Space Probe"]`) and
   the dedup set in `load_skills`. Changing `Kopi Pagi` to `kopi pagi` keeps
   the slug `kopi-pagi` but orphans the config entry, silently resetting the
   skill's enabled state. Renaming is out of scope for this route; refuse with
   400 rather than half-apply it.
3. **No collision, on both keys.** On `POST`, check **name and slug
   separately, across every read root**:
   - the manifest `name` must not match any loaded skill — `load_skills`
     dedupes by name with the first root winning, so an unchecked create
     silently shadows a skill elsewhere and that skill stops working with no
     error anywhere;
   - the derived slug must not match an existing directory in any root — two
     directories with the same slug and different names both load, and the
     API then has two skills answering to one address.

   Checking only one key leaves the other collision reachable.
4. **Slug is valid.** Reuse `validate_slug` on the `{slug}` path parameter
   exactly as the sibling routes do.

### Writing

Write to a temporary file in the same directory, then rename over `SKILL.md`.
A partial write leaves a truncated body that still parses as *something*, and
that something becomes the agent's instructions on the next reload.

`POST` creates the directory under `profile.skills_dir()` — the same root
`author_skill` uses (`src/tools/mod.rs:374`) — and writes `.origin.json` with
`kind: "authored"` via plan 052's `write_origin`. If the marker write fails
the skill is still created (per 052's rule), but it will not be editable; log
at `warn`.

## Steps

### Step 1 — widen the two helpers

`slugify` and `parse_yaml_frontmatter` to `pub(crate)`. No behaviour change,
no signature change.

**Verify**: `cargo build` clean; their existing tests still pass.

### Step 2 — slug on the list route, and slug addressing

`skill_status_json` (`api_v1.rs:1050`) gains two fields:

- `"slug"` — from `location.parent().file_name()`, but **only when the
  manifest file is named `SKILL.md` or `SKILL.toml`**. Otherwise omit it: the
  skill has no directory of its own and is neither addressable nor editable.

  The file-name check is not defensive padding. Open-skills entries
  (`load_open_skills`, `mod.rs:1220-1232`) are flat `.md` files sitting
  directly in `~/open-skills`, and they *do* carry a `location` — so a naive
  `location.parent().file_name()` returns `open-skills` for **every one of
  them**, giving dozens of skills the same address. Testing for an absent
  `location` does not catch this; testing the manifest file name does.
- `"origin": {"kind": ...}` — absent when unknown. The console needs it to
  decide which cards get a pencil without fetching each skill individually.

Then switch `skills_set_enabled` and `skills_uninstall` to resolve their path
parameter as a slug. Keep their `validate_slug` guard untouched.

**Verify**: `GET /api/v1/skills` reports slug and origin per skill; a skill
with no marker omits `origin` rather than guessing; **`PUT .../{slug}/enabled`
and `DELETE .../{slug}` now succeed for a skill whose manifest name contains
a space** (the case that returns 400 today — this is the regression test for
the bug this step fixes); ClawHub skills, whose slug and name are identical,
behave exactly as before; with `open_skills_enabled = true` and two or more
entries present, **no two skills report the same slug** and every open-skills
entry omits it.

### Step 3 — `GET /api/v1/skills/{slug}/content`

Resolve by directory slug via `load_skills_with_status` (the resolver
`skills_show` uses — it returns disabled skills too, which must stay
editable). Apply `require_authored`. Read the file, return
`{ "slug": ..., "name": ..., "content": "..." }`.

**Verify**: 200 with exact file bytes for an authored skill; 403 for a
ClawHub/bundled/unmarked skill; 404 for an unknown slug; 401 unauthenticated;
200 for an authored skill whose display name contains a space.

### Step 4 — `PUT /api/v1/skills/{slug}/content`

Body `{ "content": "..." }`. Apply `require_authored`, then validation checks
1, 2 and 4. Write atomically. Return `{ "slug": ..., "written": true }`.

**Verify**: round-trip changes the file on disk; unparseable frontmatter →
400; a body renaming the skill → 400 and the file on disk is unchanged; a body
changing only the *case* of the name → also 400 (the config key would orphan);
non-authored → 403; oversized body → 413 from the middleware.

### Step 5 — `POST /api/v1/skills`

Body `{ "name": "...", "content": "..." }`. Derive the slug with `slugify`;
an empty result is **400**. Run validation checks 1 and 3. An existing name or
slug is **409** — this route creates; `PUT` updates. Write `SKILL.md` then
`.origin.json`. Return `{ "name": ..., "slug": ..., "created": true }` with
**201**.

`name` in the body is the display name and is **not** required to equal the
`name:` inside `content` — but if they differ, the body's `content` wins,
since that is what the loader will read. Derive the slug from the effective
name (the one in `content`), not the envelope, or the directory and the
manifest disagree from the moment of creation.

**Verify**: creates a loadable skill that `GET /api/v1/skills` then lists with
origin `authored` and a slug matching its directory; a name colliding with a
skill in *another* root → 409; an existing slug with a different name → 409;
a name that slugifies to empty → 400; an envelope name that disagrees with
the content's `name:` produces a directory matching the content's name.

## STOP conditions

- Any content route reachable without `check_auth`.
- Any content route acting on a skill whose origin is not `authored`.
- Raising `MAX_BODY_SIZE`, or exempting these routes from the body-limit layer.
- A non-atomic write to `SKILL.md`.
- `POST` writing anywhere other than `profile.skills_dir()`.
- Collision checking that looks at only one root, or at only one of
  name/slug.
- Removing or weakening `validate_slug` on any route rather than feeding it a
  real slug.
- Changing the `[skills.entries.<key>]` config key from name to slug.
- Adding rename, fork, or bulk-body routes.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib gateway::api_v1
```

Live, against a paired gateway (mirror the auth setup used by the existing
skills-route tests):

```bash
# create — display name has a space; slug must not
curl -sS -X POST localhost:PORT/api/v1/skills -H "$AUTH" \
  -d '{"name":"Kopi Pagi","content":"---\nname: Kopi Pagi\ndescription: t\n---\n\n# x\n"}'
# read back by SLUG, not by name
curl -sS localhost:PORT/api/v1/skills/kopi-pagi/content -H "$AUTH"
# the bug this plan fixes: both of these 400 before the change
curl -sS -X PUT localhost:PORT/api/v1/skills/kopi-pagi/enabled -H "$AUTH" \
  -d '{"enabled":false}'
curl -sS -X DELETE localhost:PORT/api/v1/skills/kopi-pagi -H "$AUTH"
# refuse a skill we do not own
curl -sS localhost:PORT/api/v1/skills/summarizer/content -H "$AUTH"   # expect 403
```

Confirm `rantaiclaw skills list` shows the created skill as `Kopi Pagi` while
its directory is `kopi-pagi`, and that `summarizer` on disk is byte-identical
afterwards.

## Rollback

Revert the commit. The three new routes disappear and the two corrected routes
return to name-keyed addressing — which means a skill whose display name
contains a space becomes undeletable over the API again, as it is today.
Skills created through the route remain on disk as ordinary authored skills,
editable by hand and removable with `rantaiclaw skills remove "<name>"`, which
never had the bug.
