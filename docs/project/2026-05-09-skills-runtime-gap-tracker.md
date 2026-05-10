# Skills + runtime gap tracker — 2026-05-09

A living checklist of what's shipped, what's still open, and what we've
explicitly decided to skip in the rantaiclaw ↔ ClawHub/OpenClaw ↔
Hermes feature comparison. Created post-v0.6.29 to keep the picture
current after a long stretch of skill-system work.

**Convention:** ✅ shipped · ⚠ partial · ❌ open · ⊘ explicitly skipped.
Each open item has an effort estimate (XS ≤ 2h · S ≤ ½ day · M ≤ 1 day
· L > 1 day). Update this file whenever a row's status changes — it's
a backlog, not a snapshot.

---

## What's shipped (skill system, v0.6.20 → v0.6.29)

### CLI surface

- ✅ `skills install <slug | url | path>` — bare slug routes to ClawHub
- ✅ `skills list` — ✓/✗ glyphs, gating reasons, install-deps hint
- ✅ `skills show <name>` — local detail
- ✅ `skills inspect <slug>` — pre-install ClawHub metadata + scan
- ✅ `skills update [<slug> | --all]` — re-pull from ClawHub
- ✅ `skills remove <name>` — symlink-safe (B2 fix)
- ✅ `skills install-deps [<slug> | --all]` — recipe runner

### TUI surface

- ✅ `/skills` — interactive picker
- ✅ `/skill` — same picker as `/skills`
- ✅ `/skill install`, `/skills install` — both open the ClawHub picker
- ✅ `/<skill-name> [args]` — direct invoke pre-fills `Use the … skill: …`
- ✅ Autocomplete dropdown surfaces installed skills as `[skill]` rows
- ✅ Hot-reload on install (no restart needed)
- ✅ Animated spinner during install (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` cycle)
- ✅ Install picker auto-jumps to `/skills` view on success
- ✅ "↵ Enter to search ClawHub" CTA when query typed but not searched
- ✅ Kind-aware placeholder: `Type query, press Enter to search ClawHub…`
- ✅ `[Install deps]` hotkey in `/skills` picker (`Ctrl+I`/Tab)
- ✅ Gated skills are visible in `/skills` with reasons and install-deps hints
- ✅ SKILL.md edits hot-reload in the running TUI

### Format / parsing

- ✅ SKILL.md YAML frontmatter (name, description, version, author, tags)
- ✅ `metadata.clawdbot.requires.{bins, env, os}` gating
- ✅ `metadata.clawdbot.install[]` recipe parsing
- ✅ Top-level `env:` block (freeride-style)
- ✅ Both `clawdbot` and `openclaw` namespaces accepted

### Config & execution

- ✅ `[skills.entries.<name>]` TOML block (OpenClaw shape)
- ✅ Per-skill `enabled` flag — loader filter
- ✅ Per-skill `env` injected into shell exec
- ✅ Per-skill `api_key.source = env | literal` resolver
- ✅ Per-skill `config.<key>` exposed as `RANTAICLAW_SKILL_<NAME>_<KEY>`
- ✅ `migrate --from openclaw` ports `openclaw.json` skills.entries → TOML

### Install recipes (in order they're tried)

- ✅ `brew install <formula>` — Homebrew
- ✅ `uv tool install <pkg>` — Python tools
- ✅ `npm install -g <pkg>` (+ pnpm/yarn variants) — Node
- ✅ `go install <module>` — bootstraps Go via brew if missing
- ✅ `download` — URL fetch plus tar.gz/zip extract (system `tar`/`unzip`, path traversal guarded)

### Tests

- ✅ 4 unit tests for `compose_skill_env` (env passthrough, literal api_key, SCREAMING_SNAKE config, disabled-skip)
- ✅ 3 unit tests for `port_skills_entries` (translation, idempotency, missing-source)
- ✅ 2 unit tests for `install_deps::pick_preferred` + `matches_os_filter`
- ✅ tmux-based TUI smoke harness (proven works on v0.6.28)
- ✅ `skills::watcher` unit test for debounced SKILL.md reload events
- ✅ `dev/ci.sh tui-smoke` gate for the tmux harness
- ✅ API v1 SSE unit tests for streamed chat and sync compatibility

---

## ⛔ Maintainer freeze (2026-05-10)

> "as i say before if this new feature forget it. im afraid this will broke the future."
>
> — maintainer, after v0.6.32 shipped

**No new features.** P3 / P4 rows below stay parked **regardless of trigger**. If any "trigger condition" surfaces, raise with the maintainer first — do **not** promote autonomously. Scope from here on:
- ✅ bug fixes in already-shipped code
- ✅ test additions for already-shipped code
- ✅ documentation corrections
- ✅ lint / warning cleanup
- ❌ anything that adds a CLI flag, config key, route, file, env var, or capability

This freeze applies until the maintainer explicitly lifts it.

---

## Open gaps (skill system)

| ID | Gap | Effort | Priority | Notes |
|---|---|---|---|---|
| **SK-01** | `download` recipe tar.gz/zip extract | XS-S | P1 | ✅ Shipped 2026-05-09 — uses system `tar`/`unzip` to avoid new runtime deps; archive entries are validated before extract. |
| **SK-02** | TUI `[Install deps]` button in `/skills` picker | M | P2 | ✅ Shipped 2026-05-09 — `/skills` now renders active and gated rows; `Ctrl+I`/Tab can reach gated install-deps rows, runs `install_deps_for(skill)` in a blocking task, and refreshes loaded skills on completion. |
| **SK-03** | Skill file watcher (auto-reload on SKILL.md change) | M | P2 | ✅ Shipped 2026-05-09 — TUI watches profile/workspace skills dirs with `notify`, debounces changes, reloads active + gated skill lists without restart, and preserves install-deps result titles during the trailing reload cooldown. |
| **SK-04** | `skills.install.preferBrew` / `nodeManager` config keys | XS | P3 | ✅ Shipped 2026-05-10 in v0.6.33-alpha — `[skills.install]` block with `prefer_brew: bool` (default true) and `node_manager: String` (default "npm"; recognises pnpm/yarn). Wired through `SelectorPrefs::from_config` into both CLI `skills install-deps` and TUI Ctrl+I dispatch. 4 new selector tests pass. |
| **SK-09** | Hermes-style agent-callable skill management tools (`skills_list`, `skill_view`, `skills_search`, `skills_install`, `skills_install_deps`) | M | P1 | ✅ Shipped 2026-05-10 in v0.6.34-alpha — closes the coherence gap where the agent didn't know about gated skills + the "ask the agent to install for me" parity gap with Hermes. Five tools registered for the LLM. Read-side (`skills_list`, `skill_view`, `skills_search`) safe by default; write-side (`skills_install`, `skills_install_deps`) approval-gated via existing ApprovalManager. 10 unit tests pass; live-tested with MiniMax — agent now correctly identifies gated `gog` and gives the install-deps fix command, searches ClawHub, views skill metadata. |
| **SK-05** | `skills.allowBundled` block list | XS | P3 | ✅ Shipped 2026-05-09 — documented `[skills.entries.<n>] enabled = false` as the equivalent; no parallel block list added. |
| **SK-06** | Multi-source skill location precedence | M | P4 | OpenClaw checks: workspace → project-agent → personal-agent → managed → bundled → extraDirs. We check 3 spots. Real users probably never hit the gap. Defer until someone asks. |
| **SK-07** | Per-agent skill allowlists for multi-agent setups | L | P4 | Out of scope until multi-agent lands as a top-level concept. |
| **SK-08** | `migrate codex` — Codex CLI skills import | M | P4 | OpenClaw has it. Niche; defer until a Codex user asks. |

## Open gaps (Hermes-flavored — explicitly **skipped**)

| ID | Gap | Verdict |
|---|---|---|
| HE-01 | Declarative `config_settings` typed schema in skill manifest | ⊘ Skip — different ecosystem; ClawHub skills don't ship this block |
| HE-02 | `config migrate` interactive walker for unset settings | Defer (Phase 2) — file edit covers it; revisit only if friction surfaces |
| HE-03 | Skill Workshop / auto-create skills from agent work | ⊘ Skip — different product direction |
| HE-04 | Self-improvement learning loop | ⊘ Skip — different product direction |
| HE-05 | `skills snapshot export` / share | ⊘ Skip — use `git` |
| HE-06 | `skills publish` / push to a tap | ⊘ Skip — use `git push` |
| HE-07 | Multi-registry `tap add owner/repo` | ⊘ Skip — ClawHub is enough |
| HE-08 | `/<skill-name>` direct invoke | ✅ Already shipped (v0.6.26) — kept because it's pure UX, not architectural divergence |

---

## Open gaps (beyond skills)

### Runtime / API

| ID | Gap | Effort | Priority | Notes |
|---|---|---|---|---|
| **RT-01** | Verify `/api/v1/agent/chat` writes to sessions.db | S | P1 | ✅ Shipped 2026-05-09 — API path now records `source = "api"` sessions with user/assistant messages and title derivation. Persistence failure is logged but does not fail a completed chat turn. |
| **RT-02** | SSE streaming on `/api/v1/agent/chat` | M | P2 | ✅ Shipped 2026-05-09 — `Accept: text/event-stream` or `?stream=1` streams `chunk`/`usage`/tool/`error`/`done` events, cancels the agent on client disconnect, persists only non-cancelled completions, and keeps the sync JSON path unchanged. |
| **RT-03** | Env var precedence below config-stored encrypted key (B8) | XS | P3 | ✅ Shipped 2026-05-09 — CLAUDE.md and runtime docs now call out the intentional precedence order. |

### Process / governance

| ID | Gap | Effort | Priority | Notes |
|---|---|---|---|---|
| **GV-01** | Doc-pass — runtime-contract refs out of sync | L | P2 | ✅ Shipped 2026-05-09 — updated commands, config, providers, runbook, troubleshooting, and bootstrap refs for sessions/API persistence, skills install-deps, env injection, install recipes, and credential precedence. |
| **GV-02** | `dev/tui-smoke.sh` permanent test harness | XS | P1 | ✅ Shipped 2026-05-09 — launches the TUI in tmux under an isolated temp profile and verifies `/skills` renders. |
| **GV-03** | CI hook for `dev/tui-smoke.sh` | S | P3 | ✅ Shipped 2026-05-09 — `dev/ci.sh tui-smoke` builds the debug binary and runs the tmux harness, skipping gracefully when host tmux/cargo is unavailable; `all` includes the stage. |
| **UP-01** | Update strategy parity with Hermes — pre-swap snapshot, rollback CLI, systemd/launchd auto-restart, `--backup` opt-in, richer `--check`, auto-update doc page | M | P1 | ✅ Shipped 2026-05-10 in v0.6.32-alpha — `rantaiclaw rollback` restores latest snapshot + previous binary; pre-swap snapshot to `~/.rantaiclaw/.update-snapshots/<UTC>/` covers config + active-profile state + persona; `update --backup` writes full-profile tar.gz; `update --check` prints release notes URL + first 12 lines of body; `docs/operations/auto-update.md` covers cron + systemd-timer + launchd patterns. 3 snapshot tests pass. |

---

## Recommendation — what to ship next

Priority cluster, smallest-first within each band:

**P1 (this drop, ~1 day total):**
1. ✅ **GV-02** — baked tmux harness into `dev/tui-smoke.sh`
2. ✅ **RT-01** — verified and fixed API session persistence
3. ✅ **SK-01** — wired tar.gz/zip extract for download recipes

**P2 (next drop, ~2-3 days):**
4. ✅ **SK-02** — TUI `[Install deps]` hotkey
5. ✅ **SK-03** — skill file watcher
6. ✅ **RT-02** — SSE streaming API
7. ✅ **GV-01** — doc-pass on runtime-contract refs

**P3+ (defer):**
8. **SK-04** — `skills.install.*` config keys (1h)
9. **HE-02** — Phase 2 `skills config` editor (1.5d)
10. ✅ **RT-03** — B8 doc note

---

## Status snapshot by version (for quick reference)

| Version | Theme | Key landings |
|---|---|---|
| v0.6.20 | CLI ↔ TUI parity | `session list/get/search/title`, `insights`, `personality show/list/set`, `skills show` |
| v0.6.21 | Control-plane API | 18 `/api/v1/*` endpoints with bearer auth |
| v0.6.22 | Bug fixes | B1 SearXNG routing · B2 skills remove symlink · B5 chat -m · B6 setup --non-interactive · B7 YAML frontmatter |
| v0.6.23 | TUI skills cleanup | Hot-reload visibility · `/skill` mirrors `/skills` · in-picker install feedback |
| v0.6.24 | Spinner | Animated Braille spinner during ClawHub install |
| v0.6.25 | OpenClaw metadata parity | `requires` gating · `[skills.entries.<n>]` config block · `skills update` · `skills inspect` |
| v0.6.26 | OpenClaw execution parity | env injection · api_key resolver · `/<skill-name>` direct invoke · `openclaw.json` migration port |
| v0.6.27 | Persona + sessions | B3 persona-to-agent wiring · B4 `agent -m` → sessions.db · env-composition tests |
| v0.6.28 | Install picker UX | "↵ Enter to search ClawHub" CTA · kind-aware placeholder · `/skill install` mirror |
| v0.6.29 | Install-deps runner | `metadata.clawdbot.install[]` parser · `skills install-deps` CLI · brew/uv/npm/go/download recipes |
| v0.6.30 | Skills runtime polish | Gated `/skills` rows · SKILL.md file watcher · tmux TUI smoke in local CI |
| v0.6.31 | Streaming API | `/api/v1/agent/chat` SSE mode · client-disconnect cancellation · install-deps title cooldown |

---

## Out-of-scope reminders

These keep getting raised in conversation; documenting once so we don't
re-evaluate every week:

- **Self-improvement / Skill Workshop** — Hermes-only, requires deep agent-loop introspection hooks, dragging in evaluation infra. rantaiclaw isn't a self-improving agent product; we're a fast/stable runtime. Don't chase.
- **`skills publish` / `snapshot export`** — `git push` and `cp -r` cover these. Adding a parallel format means keeping it in sync.
- **Multi-registry `tap` system** — ClawHub aggregates. A second registry doesn't currently exist worth supporting.
- **Local malware scanner** — ClawHub already runs LLM + static + VirusTotal scans server-side, surfaced via `skills inspect`. Local scanning means binary bloat for redundant work.
