# Installer UX Upgrade — Design

**Date:** 2026-04-25
**Status:** Design (awaiting implementation plan)
**Scope:** Bring the RantaiClaw installer UX to feature-parity-or-better with Hermes Agent's installer. Two workstreams: (1) bash bootstrap UX layer (colors, glyphs, banners, step counter, spinner, pipe-safe prompts, `NO_COLOR` support); (2) `dialoguer`-based onboarding wizard polish (palette, framed section headers, welcome banner, completion banner). Out of scope: ratatui rewrite of the wizard, replacing `dialoguer`/`console` deps, new wizard pages, animated progress bars, the POSIX-sh shim `rantaiclaw_install.sh`.

---

## 1. Motivation

The Hermes Agent installer (`NousResearch/hermes-agent`) is widely perceived as more polished than RantaiClaw's. Concrete delta after side-by-side inspection:

| Layer | Hermes | RantaiClaw (today) |
|---|---|---|
| Bash bootstrap | ANSI colors, Unicode glyphs (`→ ✓ ⚠ ✗`), framed open + close banners, pipe-safe `[ -t 0 ]` + `< /dev/tty` prompts | plain `==>` text, no colors, no banners, `read -r -p` (breaks under `curl … \| bash`) |
| Wizard | Python `rich` — line-based but heavily styled; framed section headers; closing screen | Rust `dialoguer` + `console::style` — line-based with light styling; plain step prints (`print_step`); no closing screen |

The "feels nicer" gap is **not** a ratatui-vs-line-based gap — Hermes' wizard is also line-based. The gap is **styling polish**, concentrated in two files: `scripts/bootstrap.sh` (1009 lines, no color layer) and `src/onboard/wizard.rs` (6181 lines, light color layer).

Ratatui was considered and rejected for the wizard: fullscreen TUIs hurt `curl … | bash` compatibility, drop scrollback on failure (worse error reporting), regress accessibility, and add testing surface for a flow run once per user. Ratatui stays where it earns its keep — the chat/agent runtime TUI.

---

## 2. Architecture

```
┌────────────────────────────────────┐
│ rantaiclaw_install.sh              │  ← unchanged (88-line sh shim)
└─────────────────┬──────────────────┘
                  ↓ exec bash
┌────────────────────────────────────┐
│ scripts/bootstrap.sh               │
│  ┌──────────────────────────────┐  │
│  │ scripts/lib/ui.sh  (NEW)     │  │  ← sourced helper library
│  │  • color detection           │  │
│  │  • info/success/warn/error   │  │
│  │  • print_banner / success    │  │
│  │  • step "N/T" "title"        │  │
│  │  • spinner_start / _stop     │  │
│  │  • prompt_yes_no / _input    │  │
│  └──────────────────────────────┘  │
│  Existing 83 info/warn/error       │
│  call sites work unchanged.        │
│  4 read prompts → prompt_*.        │
│  Step labels at phase transitions. │
│  Spinner around long shell calls.  │
└─────────────────┬──────────────────┘
                  ↓ exec rantaiclaw onboard --interactive
┌────────────────────────────────────┐
│ src/onboard/wizard.rs              │
│  ┌──────────────────────────────┐  │
│  │ src/onboard/ui.rs  (NEW)     │  │  ← new module
│  │  • palette via console::Style│  │
│  │  • info/success/warn/error   │  │
│  │  • print_section_header      │  │
│  │  • print_welcome_banner      │  │
│  │  • print_completion_banner   │  │
│  └──────────────────────────────┘  │
│  Existing dialoguer flow kept.     │
│  print_step → wraps section_header.│
│  run_wizard ends with completion.  │
└────────────────────────────────────┘
```

Bash and Rust palettes are intentionally identical (cyan/green/yellow/red/magenta + bold) so the visual handoff between phases reads as one product.

---

## 3. Bash UX layer (`scripts/lib/ui.sh`)

New file, sourced by `scripts/bootstrap.sh` near the top after `set -euo pipefail`:

```bash
# shellcheck source=lib/ui.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib/ui.sh"
```

If the source fails (e.g., `ui.sh` missing in a partial checkout), `bootstrap.sh` defines minimal inline fallback helpers (`info`/`warn`/`error` as plain `echo`) so the install path never breaks.

### 3.1 Color detection

Colors enabled iff **all** of:
- `[ -t 1 ]` (stdout is a TTY)
- `NO_COLOR` env var unset (per [no-color.org](https://no-color.org/))
- `TERM` is not `dumb` and not unset

Detection runs once at `ui.sh` source time; result stored in `__UI_COLOR=1|0`. Color vars (`__UI_RED`, `__UI_GREEN`, etc.) are set to ANSI escapes when `__UI_COLOR=1`, empty strings otherwise. All helpers use these vars unconditionally — no per-call branching.

### 3.2 Glyphs

Always emitted as Unicode (`→ ✓ ⚠ ✗ •`). Modern terminals (xterm, alacritty, kitty, wezterm, gnome-terminal, iTerm2, Windows Terminal) all render these. ASCII fallback adds branching with no clear consumer in 2026 — declined.

### 3.3 Helpers

| Function | Glyph | Color | Stream | Notes |
|---|---|---|---|---|
| `info "msg"` | `→` | cyan | stdout | replaces existing `info` |
| `success "msg"` | `✓` | green | stdout | NEW |
| `warn "msg"` | `⚠` | yellow | stderr | replaces existing `warn` |
| `error "msg"` | `✗` | red | stderr | replaces existing `error` |

Names match existing helpers, so all 83 existing call sites in `bootstrap.sh` work unchanged. `success` is added at notable completion points (deps installed, build complete, install complete) — case-by-case in the implementation plan.

### 3.4 Banners

```bash
print_banner       # called once at start of bootstrap.sh main flow
print_success_banner "<line1>" "<line2>" ...   # called at end on full success
```

Layout (magenta + bold for opening, green + bold for closing):

```
┌─────────────────────────────────────────────────────────┐
│            ⚙ RantaiClaw Installer                       │
├─────────────────────────────────────────────────────────┤
│  Multi-Agent Runtime for Production AI Employees        │
└─────────────────────────────────────────────────────────┘
```

Box width is fixed (59 columns) to match Hermes' style; no dynamic `tput cols` resizing (avoids edge cases on non-TTY widths). Banners auto-suppress when `__UI_COLOR=0` AND `[ -t 1 ]` is false, falling back to a single plain heading line.

### 3.5 Step counter

```bash
step "3/7" "Installing system deps"
# prints: [3/7] Installing system deps
```

Bold cyan prefix `[N/T]`, plain text after. Called at major phase transitions only — not inside loops. The total step count `T` is computed once in `bootstrap.sh` based on CLI flag combination (e.g., `--skip-build` removes a step, `--install-system-deps` adds one). A small `compute_step_total` function lives in `bootstrap.sh` (not `ui.sh`) since it's bootstrap-specific.

### 3.6 Spinner

```bash
spinner_start "Building rantaiclaw"
# ...long-running command...
spinner_stop "Built rantaiclaw"   # success message after; or
spinner_stop_fail "Build failed"  # error variant
```

Implementation: `spinner_start` backgrounds a subshell that prints a braille spinner frame (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`) every 100ms with `\r` carriage return + the message. PID stored in `__UI_SPINNER_PID`. `spinner_stop` kills that PID, prints `\r\033[K` to clear the line, then prints `success` or `error`. An EXIT trap kills the spinner if bootstrap aborts mid-step.

Falls back to a plain `info "Building rantaiclaw…"` when `__UI_COLOR=0` or stdout not a TTY (subshell + carriage-return not safe under non-interactive logs).

Used at: `cargo build --release --locked`, `cargo install --path . --force --locked`, prebuilt-binary download, docker pull. Not used inside loops or for fast operations (< 1s).

### 3.7 Pipe-safe prompts

```bash
IS_INTERACTIVE=$(if [ -t 0 ]; then echo true; else echo false; fi)

prompt_yes_no "Run onboarding after install?" "yes"   # returns 0/1
prompt_input "Provider" "openrouter"                   # echoes value
prompt_input_secret "API key (hidden)"                 # -s flag, echoes value
```

Pattern (mirrors Hermes `prompt_yes_no`):

```bash
if [ "$IS_INTERACTIVE" = true ]; then
  read -r -p "$prompt" answer || answer=""
elif [ -r /dev/tty ] && [ -w /dev/tty ]; then
  printf "%s " "$prompt" > /dev/tty
  IFS= read -r answer < /dev/tty || answer=""
else
  answer=""   # use default
fi
```

Replaces the 4 existing `read -r -p` sites in `bootstrap.sh` (lines 329, 482, 490, 499). `prompt_input_secret` covers the API-key entry case (line 499 currently uses `read -r -s -p`).

---

## 4. Rust wizard polish (`src/onboard/ui.rs`)

New module, declared in `src/onboard/mod.rs`:

```rust
pub(crate) mod ui;
```

Lives next to `wizard.rs`; consumed by `wizard.rs`. No public re-exports outside the `onboard` module.

### 4.1 Palette

Uses `console::Style` (already a dep via `dialoguer`):

```rust
pub fn cyan() -> Style    { Style::new().cyan() }
pub fn green() -> Style   { Style::new().green() }
pub fn yellow() -> Style  { Style::new().yellow() }
pub fn red() -> Style     { Style::new().red() }
pub fn magenta() -> Style { Style::new().magenta() }
pub fn bold() -> Style    { Style::new().bold() }
```

Functions (not constants) because `Style` isn't `const`-constructible. `console` already respects `NO_COLOR` and TTY detection internally — no additional logic needed.

### 4.2 Helpers (mirror bash names)

```rust
pub fn info(msg: &str)    // → cyan glyph + msg
pub fn success(msg: &str) // ✓ green glyph + msg
pub fn warn(msg: &str)    // ⚠ yellow glyph + msg
pub fn error(msg: &str)   // ✗ red glyph + msg
```

All print to stdout (consistent with existing wizard output, which is interactive).

### 4.3 Section headers

```rust
pub fn print_section_header(current: u8, total: u8, title: &str)
```

Output (cyan + bold frame, plain title):

```
┌─ Step 3/7: Provider Selection ─────────────────────────┐
```

Replaces the existing `print_step` function (line 1615). Existing `print_step` becomes a thin wrapper that calls `print_section_header` for compat — or all call sites get updated, decided at plan-writing time based on call-site count and ergonomics.

### 4.4 Welcome banner

```rust
pub fn print_welcome_banner()
```

Printed once at the top of `run_wizard` — short framed box marking the handoff from bash:

```
┌─────────────────────────────────────────────────────────┐
│            ⚙ RantaiClaw Setup Wizard                    │
└─────────────────────────────────────────────────────────┘
  Let's get you configured. Press Ctrl-C to abort at any time.
```

Magenta + bold, matching the bash `print_banner`. Same fixed 59-col width.

### 4.5 Completion banner

```rust
pub fn print_completion_banner(next_steps: &[&str])
```

Printed at the end of a successful `run_wizard` (and, if it makes sense, `run_quick_setup` and `run_channels_repair_wizard`):

```
┌─────────────────────────────────────────────────────────┐
│            ✓ Setup Complete!                            │
└─────────────────────────────────────────────────────────┘
  → Next steps:
    • rantaiclaw chat       — start an interactive session
    • rantaiclaw agent      — run the autonomous agent loop
    • rantaiclaw status     — verify installation
```

Green + bold frame; cyan arrow + bullets for next steps. Caller passes the next-step list (so quick-setup and full-wizard can have different hints if needed).

### 4.6 What does NOT change

- `dialoguer::{Confirm, Input, Select}` calls — all preserved
- Model fetching (`fetch_openrouter_models`, `fetch_anthropic_models`, `fetch_gemini_models`, `fetch_ollama_models`) and their parsers
- Model caching (`load_model_cache_state`, `save_model_cache_state`, etc.)
- `run_quick_setup` and its sub-functions
- `run_channels_repair_wizard`
- Hardware peripheral config flow
- Config-overwrite safety (`ensure_onboard_overwrite_allowed`, `--force`)
- Provider/memory backend defaults
- The `Onboard` subcommand flag surface (`--interactive`, `--force`, `--channels-only`, `--api-key`, `--provider`, `--model`, `--memory`)

This is a presentation-only change. Any logic edit is out of scope and must be a separate spec.

---

## 5. Out of scope

- POSIX-sh shim `rantaiclaw_install.sh` stays minimal (88 lines, no colors, no banner). It's bash-free intentionally and only auto-installs bash before re-execing into `bootstrap.sh`.
- Ratatui rewrite of the wizard. Settled — see motivation §1.
- Replacing or removing `dialoguer` or `console` dependencies.
- New wizard pages, new providers, new memory backends, or any other feature work in `wizard.rs`.
- Animated progress bars (only the spinner).
- Windows installer (no `install.ps1` equivalent — RantaiClaw relies on Docker / WSL / source build).
- Localization of installer strings.

---

## 6. Testing & validation

### 6.1 Bash

- `bash -n scripts/bootstrap.sh scripts/lib/ui.sh` — syntax check (already part of validation matrix per CLAUDE.md §8).
- `shellcheck scripts/bootstrap.sh scripts/lib/ui.sh` if available.
- Manual smoke tests, three scenarios:
  1. `./rantaiclaw_install.sh --guided` in a clean interactive shell — full color path.
  2. `NO_COLOR=1 ./rantaiclaw_install.sh --guided` — no colors, glyphs still present.
  3. `curl -fsSL <raw-url>/scripts/bootstrap.sh | bash -s -- --guided` — pipe-safe prompts via `/dev/tty`.

### 6.2 Rust

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test` — existing wizard tests (especially `run_quick_setup_with_home` callers) keep passing. UI helpers are pure-format with no side effects on `Config`.
- New unit tests for `src/onboard/ui.rs`:
  - `info`/`success`/`warn`/`error` produce expected glyph + text (color stripping via `console::strip_ansi_codes` in test).
  - `print_section_header` produces expected frame and step format.
  - `print_completion_banner` includes all passed next-step strings.

### 6.3 CI

- `./dev/ci.sh all` (Docker-based local CI, recommended per CLAUDE.md).
- No new GitHub Actions workflows.

---

## 7. Risk + rollback

**Risk tier: Medium.** Touches the install path (high blast radius — gates first-run for new users) but only the presentation layer. No logic changes, no schema changes, no security-sensitive surfaces (per CLAUDE.md §5). All four high-risk paths (`src/security/**`, `src/runtime/**`, `src/gateway/**`, `src/tools/**`, `.github/workflows/**`) are untouched.

**Specific risks:**

1. **Spinner subshell leaks** — if `spinner_stop` is missed on an early `exit`, the background subshell can dangle. Mitigation: EXIT trap in `bootstrap.sh` calls `spinner_stop` defensively.
2. **Color escapes leaking to logs** — if a user redirects to file, ANSI codes can pollute the log. Mitigation: `[ -t 1 ]` detection; only emit colors when stdout is a TTY.
3. **`/dev/tty` unavailable** — on some sandboxed runners (some CI containers), `/dev/tty` doesn't exist. Mitigation: prompts fall back to default value silently when no TTY available; bootstrap continues non-interactively.
4. **`ui.sh` missing in partial checkout** — fallback inline helpers in `bootstrap.sh` (defensive `if ! source ui.sh; then define_minimal_helpers; fi`).

**Rollback:** single-PR revert. Helpers are additive; existing semantics preserved (function names, side effects, exit codes). No config schema changes, no DB migrations, no flag changes.

---

## 8. Open questions

None — design is finalized at this point. Any additional polish (e.g., spinner used in additional places, bullet-list helper, secondary banners) is left to the implementation plan and reviewable at PR time without re-spec.

---

## 9. References

- Hermes Agent installer: `NousResearch/hermes-agent` `scripts/install.sh` (lines 19–165 — color vars, banner, log helpers, `prompt_yes_no`)
- RantaiClaw bootstrap: `scripts/bootstrap.sh` (1009 lines; helpers at lines 4–14; reads at 329, 482, 490, 499)
- RantaiClaw wizard: `src/onboard/wizard.rs` (6181 lines; `print_step` at 1615; uses `dialoguer` + `console::style`)
- CLAUDE.md §3 (engineering principles), §5 (risk tiers), §8 (validation matrix)
