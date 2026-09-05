# Plan 055: Write and edit your own skills from the TUI via `$EDITOR`

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report. When done, update the status row for this plan in `plans/README.md`.
>
> **Do not build a form.** The TUI hands the file to the user's own editor and
> takes it back. If you find yourself adding text inputs, field navigation, or
> a multi-pane editing widget to the TUI, stop and re-read "Why `$EDITOR`".
>
> **Drift check (run first)**:
> `git diff --stat 6004757..HEAD -- src/tui/app.rs src/tui/commands/skills.rs src/tui/commands/mod.rs`
> Confirm `run_external_editor` still exists at roughly `app.rs:6696` and
> still ends by restoring the alternate screen.

## Status

- **Priority**: P2
- **Effort**: S–M
- **Risk**: MED (`src/tui/**` — suspends and restores the terminal)
- **Depends on**: 052 (needs `Skill.origin`). **Not** 053 — the TUI reads and
  writes the file directly and never goes through the gateway.
- **Parallel with**: 053, 054
- **Category**: feature
- **Planned at**: commit `6004757`, 2026-07-31

## Why `$EDITOR`

The web console gets a form because browser users have no text editor. TUI
users are already sitting in a terminal with the editor they use every day.
Building a form inside the TUI means building a worse editor inside their
editor.

This is the established idiom for exactly this shape of problem —
`crontab -e`, `kubectl edit`, `git commit` all hand the buffer to `$EDITOR`
rather than growing their own.

Two concrete reasons beyond idiom:

**Pasting.** Dropping a prepared `SKILL.md` into the TUI composer is the most
fragile input path in this project — paste shredding, caret drift, and
wrapped-row collapse have all been fixed there in turn. With `$EDITOR` the TUI
is fully suspended while the user pastes; it never sees a single character.
The entire failure class is bypassed rather than defended against.

**Fidelity.** Asking the agent to write the skill (`author_skill`) means the
model rewrites the content in its own style. A user who already has the exact
text they want needs a path that preserves bytes.

## Current state

The machinery already exists. `run_external_editor` (`src/tui/app.rs:6696`)
already:

- resolves the editor (`$EDITOR` → `$VISUAL` → `nano` → `vi` → `notepad`)
- writes a temp file in the OS temp dir with a pid+nonce name
- leaves the alternate screen and drops raw mode
- runs the editor with inherited stdio
- restores the terminal via `enter_fullscreen` on return
- is best-effort: on any failure the original buffer survives and the cause
  is surfaced in the status bar

It is currently hardwired to `app.context.input_buffer` at both ends.

Command dispatch returns a `CommandResult` (`src/tui/commands/mod.rs:34`) and
the side effect runs in `run_loop` — the same split the composer's editor
handoff already uses (`app.rs:1198`: "The actual swap happens in `run_loop`").

`/skill <name>` dispatch lives at `src/tui/commands/skills.rs:157` and already
pattern-matches a leading subcommand (`install`) before falling through to the
name lookup.

## Design

```
/skill new "Kopi Pagi"     open $EDITOR on a template, then create
/skill edit kopi-pagi      open $EDITOR on an existing authored skill
```

### Flow

1. **Resolve and refuse early.** Before any file is touched: `new` rejects a
   name that collides with any loaded skill, or that slugifies to empty;
   `edit` rejects a skill that is not found or whose origin is not `Authored`.
   Failing here costs the user nothing — no editor opened, no temp file.
2. **Stage.** Write the template (`new`) or the existing `SKILL.md` (`edit`)
   to a temp file.
3. **Suspend and edit.** Same handoff as the composer.
4. **Validate.** Frontmatter parses, and `name:` is present and non-empty.
   For `edit`, `name:` must equal the current name **exactly** — the name is
   the `[skills.entries.<name>]` config key, so changing even its case orphans
   the entry and silently resets the skill's enabled state. Renaming is not
   supported here; say so rather than half-applying it. For `new`, re-check
   both collisions — the user may have changed `name:` inside the editor, and
   the slug is derived from what the file actually says.
5. **Commit.** Write atomically (temp file in the destination directory, then
   rename). For `new`, also write `.origin.json` with `kind: "authored"`.
6. **Reload.** Refresh the skill list so the change is live without a restart.

### Never discard the user's work

Step 4 can fail after the user has spent real effort — a typo after pasting
200 lines must not mean starting over.

**Reopen the editor immediately with the text intact and the reason at the
top**, as a comment line the user deletes or ignores:

```
# ✗ frontmatter tidak bisa dibaca (baris 3) — perbaiki lalu simpan lagi,
#   atau keluar tanpa menyimpan untuk membatalkan.
---
name: Kopi Pagi
...
```

This is the `git commit` pattern, and it needs no new state: no `--resume`
flag, no "most recent staged file" bookkeeping, no session memory of an
abandoned edit. Loop until the content validates or the user exits without
saving, in which case report the staged path once and stop.

Cap the loop (three attempts is plenty) so a user who cannot get past
validation is not trapped in a reopening editor; on the final failure, report
the staged path and give up.

### Refuse while a turn is in flight

Suspending the terminal mid-stream leaves partial output on a screen that is
about to be torn down, and the agent keeps writing into a terminal the editor
now owns. If a turn is active, refuse with a short message and let the user
retry — do not queue, do not cancel the turn.

## Steps

### Step 1 — make the editor handoff reusable

Extract the body of `run_external_editor` into a helper that takes the initial
text and returns the edited text:

```rust
fn edit_text_in_external_editor(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    initial: &str,
    file_stem: &str,          // "prompt" | "skill" — names the temp file
) -> Result<(String, PathBuf)>   // edited text + the staged path
```

`run_external_editor` becomes a thin wrapper that calls it with
`app.context.input_buffer` and assigns the result back.

**This must not change composer behaviour in any way.** Same editor
resolution, same temp-file naming scheme, same terminal restore, same
best-effort error handling.

**Verify**: `cargo build` clean; the composer's editor handoff still works
end-to-end under tmux (see "Verification").

### Step 2 — `CommandResult::OpenSkillInEditor`

New variant carrying what `run_loop` needs:

```rust
OpenSkillInEditor {
    slug: String,
    path: PathBuf,      // destination SKILL.md (may not exist yet)
    initial: String,    // template, or the current file contents
    is_new: bool,
}
```

Dispatch builds it; `run_loop` performs the suspend, validation and commit.
Keep the policy decisions (collision, origin gate) in dispatch where they can
be unit-tested without a terminal.

**Verify**: `cargo build` clean; the new variant is handled exhaustively
wherever `CommandResult` is matched.

### Step 3 — `/skill new`

Parse `new` as a subcommand in `SkillCommand::execute`, mirroring the existing
`install` prefix match. The remainder is the display name; strip surrounding
quotes if present. Derive the slug with `slugify` (plan 053 widens it to
`pub(crate)`; if 053 has not landed, widen it here — it is the same one-word
change and must not be duplicated).

Reject: empty name, empty slug, collision with a loaded skill's **name**, and
collision with an existing **directory** at that slug — in any read root.
Two different display names can slugify to the same directory, so checking one
key leaves the other reachable.

Template: frontmatter with `name`, empty `description`, empty `tags`, an `H1`,
and an `## Instructions` heading with one empty bullet — the shape plan 054's
Form view expects, so a skill written here stays form-editable in the console.

**Verify**: unit tests on dispatch for each rejection; live run creates a
loadable skill with an `authored` marker.

### Step 4 — `/skill edit`

Search active skills then `available_skills_with_status`, so a **disabled**
skill is still editable.

**Accept either the display name or the directory slug.** The shared
`normalise_skill_name` (`commands/mod.rs:28`) is only
`to_lowercase().replace('-', "_")` — it does not touch spaces, so `Kopi Pagi`
normalises to `kopi pagi` while the directory `kopi-pagi` normalises to
`kopi_pagi`. They do not match. Since a skill created by this feature has a
spaced display name and a hyphenated directory, and the directory is what the
user sees in the filesystem and in the console, `/skill edit kopi-pagi` must
work.

Resolve in two passes: `normalise_skill_name` against `skill.name` first
(preserving today's behaviour exactly), then against the directory slug from
`location.parent().file_name()`.

**Do not change `normalise_skill_name` itself.** Its doc comment records that
`/skill <name>`, the `/skills <name>` preselect, and the `/<skill>`
direct-invoke fallback were deliberately unified on one rule after each having
a subtly different one. Widening it here would silently change all three.

Refuse unless `origin` is `Authored`, naming the actual origin in the message
so the user knows why:

```
✗ weather dikelola ClawHub — tidak bisa diedit di sini.
```

**Verify**: an authored skill opens by display name *and* by directory slug; a
bundled, ClawHub, symlinked, or `<workspace>/skills/` skill is refused; a
disabled authored skill still opens; `/skill <name>` (no subcommand) resolves
exactly as it does today.

### Step 5 — validate, commit, reload

Implement steps 4–6 of the flow in `run_loop`. Reuse
`parse_yaml_frontmatter` for validation rather than a second parser.

Reload through the same path `/skills` already uses to refresh
`available_skills` after an install — do not add a second refresh mechanism.

**Verify**: editing changes the file and the change is live in the next turn
without restarting; a broken save leaves the original file untouched and
reports the staged path.

### Step 6 — discoverability

Add `new` and `edit` to `/skill`'s help text and to the command autocomplete
list. A feature nobody can find is not shipped.

**Verify**: typing `/skill ` shows both.

## STOP conditions

- Any text-input widget, field navigation, or editing pane added to the TUI.
- `run_external_editor`'s composer behaviour changing in any observable way.
- Changing `normalise_skill_name` — it is shared by three entrypoints.
- A validation failure deleting or overwriting the staged temp file.
- A reopen loop with no attempt cap.
- `edit` accepting a skill whose origin is not `Authored`.
- `new` writing outside `profile.skills_dir()`.
- A non-atomic write to `SKILL.md`.
- The editor opening while an agent turn is streaming.
- A second skill-list refresh mechanism.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib tui::commands::skills
```

Live, under tmux with a scratch `HOME` (the established pattern for TUI work —
`send-keys` / `paste-buffer` / `capture-pane`; `sleep` is blocked, use
`read -t`). `EDITOR=nano` keeps the driving simple:

1. `/skill new "Kopi Pagi"` → nano opens on the template → **paste** a
   multi-line body with `paste-buffer` → save+exit → skill listed as
   `Kopi Pagi` in directory `kopi-pagi`, marker is `authored`, file bytes
   match exactly what was pasted.
2. `/skill edit kopi-pagi` → resolves by slug → change one line → save →
   confirm on disk. Then `/skill edit "Kopi Pagi"` → resolves by display name.
3. `/skill edit summarizer` → refused, naming `bundled`.
4. Break the frontmatter in the editor → save → editor **reopens** with the
   broken text plus the reason as a comment; fix it → saves. Then repeat and
   exit without saving → original file unchanged, staged path reported once.
5. Fail validation three times → loop stops, staged path reported.
6. Start a long turn, then `/skill edit` mid-stream → refused, stream
   continues undisturbed.
7. Regression: the composer's own `$EDITOR` handoff still works, and
   `/skill <name>` with no subcommand still opens the info panel.

## Rollback

Revert the commit. `/skill new` and `/skill edit` disappear; `/skill`,
`/skills`, and the composer's editor handoff are unchanged. Skills created
through it stay on disk as ordinary authored skills, editable by hand and via
plans 053/054.
