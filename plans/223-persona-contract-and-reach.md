# Plan 223: Make persona real end-to-end — one setter, a slug decoder, input validation, a preset route, a settable timezone, live reach to channels, and no dead SYSTEM.md

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**: `git diff --stat 8503328..HEAD -- src/persona/ src/gateway/api_v1.rs src/agent/prompt.rs src/agent/agent.rs src/channels/mod.rs src/channels/dispatch.rs src/tui/commands/skills.rs src/tui/app.rs src/onboard/section/persona.rs src/onboard/provision/persona.rs src/identity.rs docs/reference/api-v1.md`
> If any in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Why this matters

Persona is the highest-privilege free text in the product — it renders first in the system prompt, above tools and safety — yet the feature is half-wired:

- The **default install renders a broken prompt**: onboarding writes `name: ""`, the default template renders `"You are a helpful general-purpose assistant for  (timezone: UTC)"`, and nothing in the console, TUI, or CLI can set the name or fix the timezone.
- **TUI `/personality` is a stub** that prints "Personality set to: X (Full integration … pending)" and writes nothing.
- The **preset slug list is hardcoded in four places in three casings** with no `from_slug`; a sixth preset is a silent `400`.
- Persona fields are injected into the prompt with **no length or control-character validation**, and the renderer's sequential `str::replace` can re-expand a value that contains a `{{placeholder}}` token.
- `SYSTEM.md` is written on every save and **read by nothing** (the prompt renders from `persona.toml`), and the two temp files use fixed names.
- A `PUT /api/v1/personality` **reaches the web chat but not running channel listeners or the TUI** — with nothing telling the operator so.
- `aieos_path` (the identity file) is read with **no size cap** and its failure is swallowed.

This plan makes persona coherent: one shared setter used by API + CLI + TUI, a decoder used everywhere, validated inputs, a preset-list route, a settable timezone, per-message persona rendering on channels (matching the existing safety-section pattern), and the dead `SYSTEM.md` write removed.

## Current state

### The five writers of `persona.toml` / `SYSTEM.md`

- `src/gateway/api_v1.rs:1905-1966` — `personality_set` (the PUT). Hand-matches slugs at `:1924-1933`; falls back to name `"RantaiClawAgent"`, timezone `"UTC"`; calls `write_persona_toml` + `render_system_md`.
- `src/persona/cli.rs:42-62` — `set(preset)`; the same fallback + write block, but only sets the preset (no name/role/tone/avoid).
- `src/onboard/section/persona.rs:39-55` and `src/onboard/provision/persona.rs:199-211` — onboarding; both write `name: String::new()`, `timezone: "UTC"`.
- `src/tui/commands/skills.rs:483-490` and `src/tui/app.rs:3236-3242` — **the stub**: message only, no write.

### The renderer — `src/persona/renderer.rs:18-47`

```rust
    let avoid_value = avoid.unwrap_or("");
    stripped
        .replace("{{name}}", name)
        .replace("{{timezone}}", timezone)
        .replace("{{role}}", role)
        .replace("{{tone}}", tone)
        .replace("{{avoid}}", avoid_value)
```

Sequential — a `name` containing `{{role}}` is expanded by the later `role` replace.

### The slug list, four copies

- `src/persona/mod.rs:59-67` — `PresetId::slug()` (snake_case), and `PresetId::ALL` at `:47-53`. **No `from_slug`.**
- `src/gateway/api_v1.rs:1924-1933` — `match preset.as_str() { "default" => …, other => return Err(err_400(...)) }`.
- `src/tui/commands/skills.rs:9-15` — kebab-case, with two bogus keys (`concise`, `verbose`) that map to no `PresetId` and two presets missing.
- `claw-ui/src/components/ops/persona-panel.tsx:13-19` — TypeScript copy (fixed by plan 229, which will fetch the route this plan adds).

### Prompt reach

- `src/agent/prompt.rs:156-173` `render_persona_section()` reads `persona.toml` from disk each call. Consumed by `PersonaSection::build` (`:204`) and `channels::build_system_prompt_with_mode`.
- `src/channels/mod.rs:731` builds the channel prompt **once** in `start_channels_with_cancellation` and stores it as `Arc<String>` (field at `:308`, set at `:890`). `src/channels/dispatch.rs:428` already re-renders the **safety** section per message via `replace_safety_section` (`src/agent/prompt.rs:367`) — **this is the pattern to copy for persona.**
- `src/agent/agent.rs:992` (TUI/agent path) rebuilds the prompt only `if self.history.is_empty()`.
- `Config` has **no** `project_context`/`timezone` field (confirmed: `derive_defaults` at `src/onboard/section/persona.rs:67-73` is a `("", "UTC")` stub whose comment says "Wave 3 will replace this" — Wave 3 never landed).

### Identity file — `src/identity.rs:170-183` and `src/agent/prompt.rs:219`

```rust
        let content = std::fs::read_to_string(&full_path).with_context(...)?;   // no size cap
```
```rust
                if let Ok(Some(aieos)) = identity::load_aieos_identity(config, ctx.workspace_dir) {  // error swallowed
```

### Conventions

- `PresetId` derives `clap::ValueEnum` + `serde(rename_all="snake_case")`; slugs are snake_case and are the on-disk + API contract. Keep them.
- Handler errors via `err_400`; tests in `api_v1.rs` `mod tests`; persona render tests in `tests/persona_rendering.rs` (hand-rolled snapshots — see its header, `:1-10`).
- Channel prompt splicing: `replace_safety_section(prompt, replacement)` returns the prompt unchanged if the section is absent (`prompt.rs:369-371`) — mirror that safety for a `replace_persona_section`.

## Commands you will need

| Purpose | Command | Expected |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Persona unit | `cargo test --lib persona` | pass |
| Persona render | `cargo test --test persona_rendering` | pass |
| Handler | `cargo test --lib api_v1::tests` | pass |
| Prompt | `cargo test --lib agent::prompt` | pass |
| Full lib | `cargo test --lib` | pass |
| Never | bare `cargo test` | — disk-constrained |

## Scope

**In scope**:
- `src/persona/mod.rs` (add `from_slug`, `apply_update`), `src/persona/renderer.rs` (single-pass, conditional name clause), `src/persona/cli.rs` (use `apply_update`)
- `src/gateway/api_v1.rs` (`personality_set` uses `apply_update` + validation; `timezone` field; `GET /api/v1/personality/presets`)
- `src/agent/prompt.rs` (`replace_persona_section` helper), `src/channels/dispatch.rs` (splice persona per message), `src/agent/agent.rs` (no change unless step 6 requires — see escape hatch)
- `src/tui/commands/skills.rs`, `src/tui/app.rs` (route the stub through the shared setter)
- `src/onboard/section/persona.rs`, `src/onboard/provision/persona.rs` (seed a real name; keep `derive_defaults` but give it a non-empty fallback)
- `src/identity.rs`, `src/agent/prompt.rs` (cap + log the aieos read)
- Delete the `render_system_md` write path (see step 5), or keep the function but stop calling it — decide per step 5.
- `docs/reference/api-v1.md` (persona section: `timezone`, presets route, reach note)

**Out of scope**:
- `always_on_kbs` retrieval internals (KB effort). This plan preserves the field through `apply_update` and returns it unchanged.
- The claw-ui persona editor (plan 229) — this plan makes the API able to set every field; the panel that sends them is 229.
- Autonomy/approval, config API — other efforts.

## Git workflow

- Branch: `fix/persona-contract-and-reach`.
- Commits: `feat(persona): add PresetId::from_slug and a shared apply_update`, `fix(persona): single-pass render and drop the empty-name clause`, `fix(api): validate persona fields and add a settable timezone`, `feat(api): serve the persona preset list`, `fix(tui): make /personality actually write the persona`, `fix(channels): re-render the persona section per message`, `fix(persona): stop writing an unread SYSTEM.md`, `fix(identity): cap the aieos file read and log a bad path`, `fix(onboard): seed a real agent name`.
- No `Co-Authored-By: Claude`. Do not push/PR unless instructed.

## Steps

### Step 1: `from_slug` + shared `apply_update`

In `src/persona/mod.rs`:

```rust
impl PresetId {
    pub fn from_slug(s: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|p| p.slug() == s)
    }
}

/// Fields a caller may change; `None` leaves the current value. `avoid` uses
/// `Some("")` to clear (renderer treats blank as none). Single source of the
/// read→default→merge→write→render sequence, shared by the API and the CLI.
pub struct PersonaUpdate {
    pub preset: Option<PresetId>,
    pub name: Option<String>,
    pub timezone: Option<String>,
    pub role: Option<String>,
    pub tone: Option<String>,
    pub avoid: Option<Option<String>>,   // Some(None)=clear, Some(Some(x))=set, None=leave
    pub always_on_kbs: Option<Vec<String>>,
}

pub fn apply_update(profile: &Profile, up: PersonaUpdate) -> Result<PersonaToml> {
    let existing = read_persona_toml(profile)?;
    let mut next = existing.clone().unwrap_or_else(|| PersonaToml::default_for(
        existing.as_ref().map(|e| e.name.as_str()).unwrap_or("RantaiClaw"),
        existing.as_ref().map(|e| e.timezone.as_str()).unwrap_or("UTC")));
    if let Some(p) = up.preset { next.preset = p; }
    if let Some(n) = up.name { next.name = n; }
    if let Some(tz) = up.timezone { next.timezone = tz; }
    if let Some(r) = up.role { next.role = r; }
    if let Some(t) = up.tone { next.tone = t; }
    if let Some(a) = up.avoid { next.avoid = a.filter(|s| !s.trim().is_empty()); }
    if let Some(k) = up.always_on_kbs { next.always_on_kbs = k; }
    write_persona_toml(profile, &next)?;
    Ok(next)
}
```

Note the default name is now `"RantaiClaw"` (not `"RantaiClawAgent"`) to match the console's brand fallback — confirm the desired single default with the operator only if it matters; otherwise use `"RantaiClaw"` consistently and update the console fallback in plan 229.

**Verify**: `cargo test --lib persona::tests::from_slug_round_trips` (assert every `PresetId::ALL` slug decodes back; an unknown slug is `None`).

### Step 2: Single-pass renderer, conditional name clause

In `src/persona/renderer.rs`, replace the five sequential `.replace` calls with a single left-to-right scan that resolves each `{{key}}` exactly once (walk the template, on `{{` read to `}}`, substitute from a fixed map, emit literal otherwise). Keep the `{{#if avoid}}` handling as is.

Add an `{{#if name}}`-style guard to `src/persona/presets/default.md:3` (and any preset whose first line embeds `{{name}}`) so an empty name drops the clause instead of rendering "assistant for  (timezone: UTC)". Simplest: change the template line to two variants the renderer picks between when `name.trim().is_empty()`. If adding a second block guard to the renderer is too invasive, instead have `render` substitute a sensible fallback (`if name.trim().is_empty() { "you" }`) — pick whichever keeps the renderer simple; the observable requirement is **no double-space "assistant for  (timezone".**

Update `tests/persona_rendering.rs`: add `renders_without_a_blank_name_gap` (render the default preset with `name=""` and assert the output does not contain `"for  ("` and does not contain `{{`). Add `placeholder_in_a_field_value_is_not_re_expanded` (name = `"{{role}}"`, role = `"analyst"`; assert the rendered output contains the literal `{{role}}` from the name, not a second `analyst`).

**Verify**: `cargo test --test persona_rendering` → new tests pass; regenerate snapshots only if a template line legitimately changed (`UPDATE_SNAPSHOTS=1 cargo test --test persona_rendering`, then review the diff).

### Step 3: Validate fields; add `timezone`

In `src/gateway/api_v1.rs`:

1. Add `#[serde(default)] timezone: Option<String>` to `PersonalityBody` (`:1883`).
2. Add a validator:
   ```rust
   fn validate_persona_field(label: &str, value: &str, max: usize) -> Result<(), (StatusCode, Json<ErrorBody>)> {
       if value.chars().count() > max { return Err(err_400(&format!("{label} exceeds {max} characters"))); }
       if value.chars().any(|c| c.is_control() && c != '\n' && c != '\t') { return Err(err_400(&format!("{label} contains control characters"))); }
       Ok(())
   }
   ```
   Caps: name 80, timezone 64, tone 80, role 400, avoid 400. Validate each supplied field before persisting. (Newlines are allowed but capped; if you want to forbid them in name/tone, add `|| c == '\n'` for those two.)
3. Rewrite `personality_set` to build a `PersonaUpdate` (preset via `PresetId::from_slug(preset).ok_or_else(|| err_400(...))`) and call `crate::persona::apply_update(&profile, update)`. Remove the hand-match block and the local fallback block. Keep the JSON response shape.
4. `personality_get`: add `timezone` (already returned) — no change needed beyond confirming it is present.

Tests (`api_v1.rs` `mod tests`, `ENV_LOCK`+`HomeGuard`): `personality_set_rejects_overlong_name` (81 chars → 400); `personality_set_rejects_control_chars` (name with `\u{07}` → 400); `personality_set_unknown_preset_is_400`; `personality_set_partial_put_preserves_other_fields` (set role only, assert name/tone unchanged); `personality_set_can_set_timezone`.

**Verify**: `cargo test --lib api_v1::tests::personality_set_` → pass.

### Step 4: Serve the preset list

Add `GET /api/v1/personality/presets` (bearer-gated) returning `[{ "id": slug, "label": …, "description": desc }]` from `PresetId::ALL` (label = a title-cased slug or a new `PresetId::label()` method — add `label()` next to `description()`). Register the route in the router (`api_v1.rs:37-79`).

Test: `personality_presets_lists_all_five` — call the handler, assert `len == PresetId::ALL.len()` and that `default` is present with a non-empty description; `personality_presets_requires_auth_when_pairing_enabled`.

**Verify**: `cargo test --lib api_v1::tests::personality_presets` → pass. Document the route in `docs/reference/api-v1.md` next to the other `/personality` routes.

### Step 5: TUI `/personality` writes

Both `src/tui/commands/skills.rs:483-490` and the `ListPickerKind::Personality` branch at `src/tui/app.rs:3236-3242`:

- Parse the requested preset with `crate::persona::PresetId::from_slug(&normalized)` (normalize the picker's kebab keys back to snake_case, or — better — change `PERSONALITY_PRESETS` at `skills.rs:9-15` to use the real slugs and drop the two bogus entries).
- On a match, call `crate::persona::apply_update(&profile, PersonaUpdate { preset: Some(p), ..Default::default() })` (add `#[derive(Default)]` to `PersonaUpdate`), then message `"Personality preset set to {slug}."`.
- On no match, message `"Unknown preset '{key}'."` — never the old "pending" text.

Because the TUI's live `Agent` only rebuilds its prompt when history is empty (`agent.rs:992`), also print a one-line note: `"Takes effect on your next new conversation (/new) or reload."` — do not attempt a live in-place prompt swap for the TUI in this plan (that is the escape hatch below).

**Verify**: `cargo test --lib tui::commands` if any command tests exist; otherwise `cargo clippy --all-targets -- -D warnings` → 0, and manual: `rtk proxy grep -n "Full integration with system prompt pending" src/` returns nothing.

### Step 6: Persona reaches running channels per message

Mirror the safety-section pattern. In `src/agent/prompt.rs` add:

```rust
pub const PERSONA_SECTION_HEADING: &str = "## Persona";

/// Swap the persona section of an already-built prompt for `replacement`
/// (or remove it when `replacement` is empty). Returns the prompt unchanged
/// when it carries no persona section, so a caller cannot lose the rest.
pub fn replace_persona_section(prompt: &str, replacement: &str) -> String { /* same shape as replace_safety_section */ }
```

In `src/channels/dispatch.rs`, next to the existing `replace_safety_section` call (`:428`), also splice the persona:

```rust
let base_prompt = crate::agent::prompt::replace_persona_section(
    &base_prompt,   // already the safety-spliced prompt
    &crate::agent::prompt::render_persona_section());
```

`render_persona_section()` reads `persona.toml` fresh (`prompt.rs:156`), so a `PUT /personality` now reaches the next channel message with no restart. It is in-memory + one small file read per message — the same cost the safety splice already accepted (see the comment at `dispatch.rs:420-427`).

Test (`agent::prompt` unit): `replace_persona_section_swaps_only_that_section` (build a prompt with `## Persona` and `## Project Context`, replace persona, assert the other section survives and the new persona text is present); `replace_persona_section_noop_when_absent`.

**Verify**: `cargo test --lib agent::prompt::tests::replace_persona_section` → pass.

### Step 7: Stop writing the unread SYSTEM.md

`rtk proxy grep -rn "SYSTEM.md\|system_md\|render_system_md" src/` — the only readers are none (writers: `api_v1.rs:1957` via `apply_update` now, `persona/cli.rs`, both onboarding files; the prompt renders from `persona.toml`). Remove the `render_system_md` calls from all writers. Keep `PersonaToml::render()` (used by `render_persona_section` and tests). Delete `render_system_md` itself and its call sites, and drop the two `tests/persona_rendering.rs` assertions that check the SYSTEM.md body (`:222-235`) — or repoint them at `persona.render()` directly. Update the onboarding "SYSTEM.md generated" message (`provision/persona.rs:217`) to "Persona saved.". Update the stale module docstring `src/persona/mod.rs:14-16` ("Wave 3 will wire…") and `src/agent/prompt.rs:186-199` (persona-was-decorative note).

If any external tool or docs reference `SYSTEM.md` as an operator-editable file, STOP and report (it would mean the file has an intended reader outside `src/`).

**Verify**: `rtk proxy grep -rn "render_system_md" src/` returns nothing; `cargo test --lib` and `cargo test --test persona_rendering` → pass.

### Step 8: Cap and log the identity read

`src/identity.rs:170-183`: bound the read — read at most `IDENTITY_MAX_BYTES` (define `const IDENTITY_MAX_BYTES: usize = 64 * 1024;`) via `read_to_string` then truncate on a char boundary, or read the file len first and bail with context if over cap. `src/agent/prompt.rs:219`: change `if let Ok(Some(aieos)) = …` to match the `Err` arm and `tracing::warn!(error = %e, "aieos identity failed to load; falling back to workspace files")`.

Test: `identity_read_is_capped` — write a >64 KiB aieos file, load it, assert the rendered identity length is bounded. (If the aieos loader is hard to unit-test in isolation, at minimum assert the const is used with `rtk proxy grep -n IDENTITY_MAX_BYTES src/identity.rs`.)

**Verify**: `cargo test --lib identity` or the grep check → pass.

### Step 9: Seed a real onboarding name

`src/onboard/section/persona.rs:67-73` `derive_defaults`: keep the signature but return a non-empty fallback — `("RantaiClaw".to_string(), "UTC".to_string())` — so a headless/interview install renders a coherent sentence. `src/onboard/provision/persona.rs:200-203`: replace `name: String::new()` with the same default (or thread the interview's name if one is collected — check whether `provision` collects a name; if it does, use it). The real fix for a *correct* timezone is out of scope (no `Config.timezone` exists); a coherent default is the deliverable here.

**Verify**: `cargo test --lib onboard` if tests exist; else `rtk proxy grep -n 'name: String::new()' src/onboard/` returns nothing.

### Step 10: Format, lint, full suite

`cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --lib`, `cargo test --test persona_rendering`.

## Test plan

Named per step. Renderer tests go in `tests/persona_rendering.rs` (match its hand-rolled snapshot style, `:1-10`). Handler tests in `api_v1.rs` `mod tests` under `ENV_LOCK`+`HomeGuard`. Prompt-splice tests in `agent::prompt` `mod tests`.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib` and `cargo test --test persona_rendering` exit 0 with the new tests present
- [ ] `rtk proxy grep -rn "render_system_md" src/` returns nothing
- [ ] `rtk proxy grep -n "Full integration with system prompt pending" src/` returns nothing
- [ ] `rtk proxy grep -rn "PresetId::from_slug" src/` shows use in `api_v1.rs` and the TUI
- [ ] `rtk proxy grep -n "replace_persona_section" src/channels/dispatch.rs` returns one match
- [ ] `rtk proxy grep -n 'name: String::new()' src/onboard/` returns nothing
- [ ] `rtk proxy grep -n "personality/presets" src/gateway/api_v1.rs` returns one match
- [ ] No files outside the in-scope list modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

- Cited excerpts do not match live code.
- `render_persona_section()` spliced per channel message turns out to be expensive because `ProfileManager::active()` mkdirs on every call (it does — `src/profile/mod.rs:111-133`). If profiling in review shows it matters, the fix is a non-mutating profile resolver; note it and still ship the splice (correctness first). Do **not** cache the persona string at channel start — that reintroduces the staleness this step fixes.
- Removing `render_system_md` breaks a test or caller outside `src/persona`, `src/onboard`, `src/gateway`, `tests/` — report it (something reads SYSTEM.md after all).
- Making `PersonaUpdate` the CLI path changes CLI `personality set` output in a way a CLI test pins — update the test to the new message, do not revert the shared setter.
- Adding `{{#if name}}` to the renderer requires a second block-guard mechanism that complicates it beyond a few lines — use the inline fallback (`"you"`) instead and note the choice.
- A step's verification fails twice after a reasonable fix.

## Maintenance notes

- The TUI still does not hot-swap persona into a *running* conversation (only `/new` or reload). Wiring that is a follow-up: it needs the same `replace_persona_section` splice on the TUI turn path (`agent.rs` around `:992`) plus a persona-file watcher — deliberately deferred to keep this PR bounded.
- A correct default timezone needs a `Config.project_context.timezone` (the never-built "Wave 3"). Until then the operator sets it via the console (plan 229) or `PUT /personality`.
- Reviewer focus: that every persona writer now goes through `apply_update`; that the channel splice reads the file fresh (no cached `Arc<String>` persona); that no SYSTEM.md writer survives.
