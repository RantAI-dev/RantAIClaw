# 075 — Memory UI/UX sweep (TUI + gateway + web console)

Written against `82df891`. Scope: memory only, both operator surfaces.
Every finding below was confirmed by running the binary against a seeded
store of 121 entries — not by reading code.

## PR A — TUI `/memory`

- **T1** `/memory recall project 2024` searched only `project`. `rsplitn`
  takes the last token as a limit, so any query ending in a number loses it,
  silently, with a result that looks correct. Replace positional limit with
  explicit `--limit N`; everything else is the query.
- **T2** `/memory list conversation` printed
  `Memory entries (121, backend: sqlite, listing the most recent 1)`.
  `count()` ignores the category filter, so the header contradicts the body.
  Report the filtered count when a filter is present.
- **T3** `/memory get` printed `2026-08-06T04:59:09.523739664+00:00`.
  Sub-second precision is noise in an operator-facing line.
- **T4** `/memory add` hardcodes `MemoryCategory::Core`. CLI, API and the web
  console can all set a category; the TUI cannot. Add `--category C`.
- **T5** `/memory recall` shows no relevance score although `memory_recall`
  (the agent-facing tool) does. Same ` [{:.0}%]` shape.

## PR B — gateway `GET /api/v1/memory`

- **W3** `?category=conversation` is accepted and ignored — 200 with mixed
  categories. Confirmed against a control (`off0` vs `off100` differ, so
  offset works; category does not). Add `category` to `ListQuery` and pass it
  to `Memory::list`.
- **W2** No recall endpoint exists at all, so the web console cannot search
  what the CLI and TUI both search. Add `?q=` routing to `Memory::recall`.

Both are additive query params on an existing route: absent behaves exactly
as today. Contract addition — call it out in the PR body.

## PR C — claw-ui memory panel (depends on B)

- **W1** Panel reads "100 of 121" and stops; the remaining 21 are
  unreachable. `api.memory(limit, offset)` already takes an offset and the
  server already honours it — the panel never passes one.
- **W2** No search box.
- **W3** No category filter, though every row shows a category badge and the
  store form has a category picker.
- **W4** Long entries clamp at three lines with no expand and no detail view.
- **W5** The store form cannot name a key though `memory_create` accepts one.
- **W6** One object, six names on a single screen: Memory / Facts / Memory
  entries / Store a fact / Fact forgotten / Forget this memory.

## Validation

Per PR: `cargo fmt --all -- --check`, `cargo test --lib <module>`,
`cargo build` (binary, not just lib), then
`BASE_SHA=$(git rev-parse origin/main) bash scripts/ci/rust_strict_delta_gate.sh`
**after committing**. Re-drive the surface live before calling it done.
