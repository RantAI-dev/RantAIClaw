# Plan 052: Record who authored each skill (`.origin.json`)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **This plan changes no user-visible behaviour.** It writes a new file next
> to `SKILL.md` and exposes one new field on `Skill`. Nothing reads that field
> to make a decision yet — plan 053 does. If you find yourself gating,
> filtering, or blocking anything on the new field, you have gone too far.
>
> **Drift check (run first)**:
> `git diff --stat 6004757..HEAD -- src/skills/mod.rs src/skills/clawhub.rs src/skills/bundled/mod.rs src/tools/author_skill.rs`
> Compare the "Current state" excerpts against live code. Line numbers drifting
> by a line or two while the quoted text matches is **not** a STOP — only a
> content mismatch is.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW (`src/skills/**` — additive; no existing path changes result)
- **Depends on**: none
- **Blocks**: 053, 054
- **Category**: feature (enabler)
- **Planned at**: commit `6004757`, 2026-07-31

## Why this matters

A user asked for a "write a skill" form in the web console, with an Edit
button on skills they wrote themselves. That button needs to know which skills
those are. Today it cannot.

Skills reach disk five ways, and three of them produce a **byte-identical
shape**: a plain directory holding `SKILL.md`.

| Source | Shape on disk | Distinguishable? |
|---|---|---|
| `author_skill` (chat) | plain dir | **no** |
| Bundled starter/core pack | plain dir | only by a hardcoded slug list |
| Third-party `cp -r` drop | plain dir | **no** |
| ClawHub install | dir + `.clawhub.json` | yes |
| Local-path install | symlink | yes |

Offering Edit on an indistinguishable directory means offering it on skills
the user does not own. Editing a bundled skill gets silently reverted by the
next `setup` run (`install_core_skills` re-seeds it). Editing a vendor-managed
skill gets overwritten by the vendor's next installer run. In both cases the
user's work disappears with no error and no warning.

The bundled slug list is not a fix. It is a hardcoded array
(`CORE_PACK`/`STARTER_PACK`, checked at `mod.rs:1488` and `mod.rs:1772`), so a
user who names their own skill `summarizer` is treated as having edited a
bundled one.

This plan makes origin **declared** rather than inferred: every write path
RantaiClaw controls records who wrote the skill, in a file beside `SKILL.md`.
Absence of that file means the origin is unknown, which downstream code must
treat as "not the user's" — failing closed, per CLAUDE.md §3.5.

### Skills that predate the marker

Every skill already on disk when this lands has no marker — including ones the
user genuinely wrote by hand, which is how skills have been made until now.
Treating all of them as "not yours" would mean the Edit button never appears
for the very skills it was asked for.

So origin resolution has two tiers, and the order is load-bearing:

```
.origin.json present?  →  use it. Authoritative. Stop.
absent?                →  infer from shape:

                          plain directory (not a symlink)
                          AND under profile.skills_dir()
                          AND no .clawhub.json
                          AND slug not in CORE_PACK/STARTER_PACK
                                        ↓
                                    Authored
```

Anything else with no marker resolves to unknown (`None`).

The inference is **only** consulted when no marker exists. A marker always
wins, including one that says something the shape would contradict.

This is deliberately transitional. The moment a skill is saved through the
editor (plans 053/055) a real marker is written, and that skill is never
shape-inferred again. The set of skills relying on inference only shrinks.

**Residual risk, stated plainly**: a third party that copies a directory
straight into `profile.skills_dir()` without going through any RantaiClaw
command gets classified `Authored`, and the user may edit content the vendor
will overwrite. Nothing does this today — the one known third-party installer
(`RantAI-Copilot`) targets `<workspace>/skills/`, which the inference excludes
— and the supported install path writes a `Local` marker, which suppresses
inference entirely. Accepted as narrow and shrinking; revisit if a vendor is
ever found dropping into the profile root unmarked.

Note the inference reuses the bundled slug list it is meant to replace. That
is fine here: it only covers bundled skills installed *before* this plan, and
`install_pack` marks every one it writes from now on. A user cannot end up
with their own skill named `summarizer` either — the create paths reject a
name that collides with any loaded skill, and bundled skills are loaded.

### What this is not

`.origin.json` is not a security boundary. It sits in a directory the operator
can already write to, so anyone with filesystem access can forge
`kind: "authored"`. That is acceptable: they can edit the skill body directly
anyway, so forging the marker grants nothing new. The marker prevents
*accidental* edits of content someone else manages. It is not a permission
check, and no code added by this plan or its successors may treat it as one.

## Current state

`Skill` carries no origin field (`src/skills/mod.rs:28-62`):

```rust
pub struct Skill {
    pub name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    ...
    #[serde(skip)]
    pub remote: bool,
}
```

ClawHub already has exactly the pattern this plan generalises
(`src/skills/clawhub.rs:571-588`):

```rust
pub(crate) const PROVENANCE_FILE: &str = ".clawhub.json";

pub(crate) struct Provenance {
    pub owner: String,
    pub slug: String,
    pub version: String,
}
```

and guards it from being shipped inside an archive
(`src/skills/clawhub.rs:598`, `is_reserved_manifest_path`).

The five write sites:

| Site | File:line | Writes to |
|---|---|---|
| `author_skill` | `src/tools/author_skill.rs:301-318` | `profile.skills_dir()/<slug>/` |
| ClawHub install | `src/skills/clawhub.rs:383,556` | `profile.skills_dir()/<slug>/` |
| Bundled pack | `src/skills/bundled/mod.rs:80-88` | `profile.skills_dir()/<slug>/` |
| Git clone | `src/skills/mod.rs:1680-1694` | `skills_dir(workspace_dir)/<repo>/` |
| Local path | `src/skills/mod.rs:1695-1760` | `skills_dir(workspace_dir)/<name>` (symlink) |

## Design

One new file per skill directory, beside `SKILL.md`:

```json
{ "kind": "authored", "source": null }
```

`kind` is a closed set. `source` records where the skill came from when that
is meaningful, and is `null` otherwise.

| `kind` | Written by | `source` |
|---|---|---|
| `authored` | `author_skill`, and (plan 053) the console create route | `null` |
| `clawhub` | `clawhub::install_one` | the `@owner/slug` reference |
| `bundled` | `bundled::install_pack` | `null` |
| `git` | `skills install <git-url>` | the clone URL |
| `local` | `skills install <path>` | the source path |
| *absent* | anything else | — |

Absent is a real, expected state — every skill installed before this lands has
no marker — and must never be an error.

### Why not extend `.clawhub.json`

`.clawhub.json` carries `owner`/`slug`/`version`, which `skills update` reads
to re-fetch the right publisher's copy. That is a different concern from
"who authored this", it is already shipped and tested, and merging the two
would mean migrating existing markers for no gain. ClawHub installs write
**both** files. Two small files, two clear jobs (CLAUDE.md §3.4).

## Steps

### Step 1 — `src/skills/origin.rs`

New module. Keep it small and pure where possible.

```rust
pub enum SkillOriginKind { Authored, Clawhub, Bundled, Git, Local }

pub struct SkillOrigin {
    pub kind: SkillOriginKind,
    pub source: Option<String>,
}

pub(crate) const ORIGIN_FILE: &str = ".origin.json";

pub(crate) fn write_origin(dir: &Path, origin: &SkillOrigin) -> Result<()>;
pub(crate) fn read_origin(dir: &Path) -> Option<SkillOrigin>;
```

`read_origin` returns `None` for a missing file, unreadable file, malformed
JSON, **and an unrecognised `kind`**. A future version writing a `kind` this
build does not know must degrade to "unknown", not to a wrong answer — so
deserialize `kind` explicitly rather than deriving `Deserialize` on the enum
with a permissive fallback.

Register the module in `src/skills/mod.rs`.

**Verify**: `cargo build` clean.

### Step 2 — reserve the filename

`is_reserved_manifest_path` (`src/skills/clawhub.rs:598`) currently rejects
`.clawhub.json`. Add `.origin.json` alongside it, keeping the existing
component-wise, case-insensitive comparison exactly as it is — a ClawHub
archive shipping its own `.origin.json` would otherwise claim any origin it
liked, including `authored`.

**Verify**: extend the existing reserved-path test with `.origin.json`,
`./.origin.json` and `.ORIGIN.JSON`. `cargo test --lib skills::` passes.

### Step 3 — write the marker at all five sites

Each site writes its own `kind` immediately after the skill directory is
populated. A failed marker write must **not** fail the install — log at `warn`
and continue. An install that succeeded but has no marker degrades to
"unknown origin", which is safe; an install rolled back because a metadata
file could not be written is not.

- `author_skill.rs` — after `fs::write(&skill_md, …)` succeeds → `Authored`
- `clawhub.rs` — beside the existing `write_provenance` call → `Clawhub`,
  `source` = the reference
- `bundled/mod.rs` — in `install_pack`, only for directories it actually
  created (it skips existing ones by design) → `Bundled`
- `mod.rs` git branch — after a successful clone → `Git`, `source` = URL
- `mod.rs` local-path branch — after symlink/junction/copy succeeds →
  `Local`, `source` = the source path

The local-path branch has four platform arms (unix symlink, windows symlink,
windows junction, copy fallback). Write the marker **once after** the arms, not
inside each — one call site, not four.

> Note for the local-path case: the marker lands in the **destination**
> directory. Where the destination is a symlink, that resolves to the target —
> so the marker is written inside the user's source tree. This is intended
> (the skill *is* that directory), but the executor must not "fix" it by
> writing outside the skill dir.

**Verify**: one test per site asserting the marker exists with the right
`kind` after the operation. For `bundled`, also assert a pre-existing
directory is left alone — including its marker, if any.

### Step 4 — resolve origin, marker first then shape

One function, and it is the only place origin is decided:

```rust
pub(crate) fn resolve_origin(dir: &Path) -> Option<SkillOrigin>
```

1. `read_origin(dir)` — if `Some`, return it unchanged. Do not second-guess it
   against the shape.
2. Otherwise apply the inference from "Skills that predate the marker". All
   four conditions must hold; any failure returns `None`.
3. `None` otherwise.

The `profile.skills_dir()` comparison must canonicalize both sides before
comparing, so a symlinked or `..`-containing path cannot be made to look like
it sits in the profile root.

Open-skills entries need no special case, but do not "fix" what looks like
one. `load_open_skills` (`mod.rs:1220-1232`) builds skills from flat `.md`
files sitting directly in `~/open-skills`, and it *does* set `location` — so
`location.parent()` is `~/open-skills`, which fails the profile-root test and
correctly resolves to `None`. It falls out of the rule; leave it there.

**Verify**: a marker saying `Clawhub` in a directory whose shape says
`Authored` resolves `Clawhub`; a plain dir in the profile root with no marker
resolves `Authored`; the same dir with a `.clawhub.json` beside it resolves
`None`; a bundled slug resolves `None`; a symlink resolves `None`; a plain dir
under `<workspace>/skills/` resolves `None`.

### Step 5 — expose it on `Skill`

Add `#[serde(skip)] pub origin: Option<SkillOrigin>`, populated by
`load_skill_md` and `load_skill_toml` via `resolve_origin` on the manifest's
parent directory. `#[serde(skip)]` mirrors `location` and `remote`: read from
disk at load time, never from the manifest itself — otherwise a `SKILL.md`
could declare its own origin.

Update every `Skill { … }` literal in tests to compile.

**Verify**: `cargo test --lib skills::` passes; a skill with no marker under
`<workspace>/skills/` loads with `origin: None`.

## STOP conditions

- Any existing test fails in a way that is not a mechanical struct-literal fix.
- Making `read_origin` return an error rather than `None` for a missing file.
- A marker write failure aborting an install.
- Any code path filtering, gating, or hiding a skill based on `origin` — that
  belongs to plan 053.
- `.clawhub.json` being changed, moved, or merged into the new file.
- The shape inference overriding, or being consulted alongside, an existing
  marker. Marker present means marker wins, unconditionally.
- Comparing paths against `profile.skills_dir()` without canonicalizing both.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib skills::
cargo test --lib tools::author_skill
```

Manual, on a scratch `HOME` (the skills loader tests are non-hermetic — see
`plans/036`):

```bash
export HOME=/tmp/origin-check && rm -rf "$HOME" && mkdir -p "$HOME"
rantaiclaw skills list                     # seeds the bundled pack
cat "$HOME"/.rantaiclaw/profiles/default/skills/summarizer/.origin.json
# expect: {"kind":"bundled","source":null}
```

Then confirm the absent case is not an error: delete that file, re-run
`skills list`, and expect the skill still listed with no warning.

Confirm the fallback both fires and stays out of the way:

```bash
# hand-made skill in the profile root → inferred Authored
mkdir -p "$HOME"/.rantaiclaw/profiles/default/skills/handmade
printf -- '---\nname: handmade\ndescription: t\n---\n\n# x\n' \
  > "$HOME"/.rantaiclaw/profiles/default/skills/handmade/SKILL.md

# same file under the workspace root → NOT inferred
mkdir -p "$HOME"/.rantaiclaw/profiles/default/workspace/skills/elsewhere
printf -- '---\nname: elsewhere\ndescription: t\n---\n\n# x\n' \
  > "$HOME"/.rantaiclaw/profiles/default/workspace/skills/elsewhere/SKILL.md
```

Assert via a unit test on `resolve_origin` (not by eyeballing `skills list`,
which does not print origin until plan 053) that the first is `Authored` and
the second is `None`.

## Rollback

Revert the commit. `.origin.json` files left on disk become unread files
inside skill directories, which every released loader ignores. No skill
changes shape, moves, or stops loading.
