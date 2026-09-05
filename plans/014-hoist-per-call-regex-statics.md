# Plan 014: Hoist per-call regex compilation to LazyLock statics (web_search + gateway history)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/tools/web_search_tool.rs src/gateway/mod.rs`
> If either changed since this plan was written, compare the "Current state"
> excerpts against the live code; on a mismatch, treat it as a STOP condition.

## Status

- **Priority**: P3
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: perf
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

Several regexes are compiled fresh on every call: 2 in the DuckDuckGo/SearXNG
HTML parsers per web search, and 2 in `extract_tool_calls_from_history` per
webhook-history parse. Regex compilation is tens of µs to low ms each and is pure
waste — the patterns are constant. The agent loop already does this right with
`LazyLock<Regex>` statics; this brings the two remaining hot-ish paths in line.
Small individually, but free correctness/perf hygiene with zero behavior change.

## Current state

- `src/tools/web_search_tool.rs` — `parse_duckduckgo_results` compiles per call
  (verified):
  ```rust
  fn parse_duckduckgo_results(&self, html: &str, query: &str) -> anyhow::Result<String> {
      let link_regex = Regex::new(r#"<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>([\s\S]*?)</a>"#)?;   // line 61
      let snippet_regex = Regex::new(r#"<a class="result__snippet[^"]*"[^>]*>([\s\S]*?)</a>"#)?;                     // line 66
      ...
  ```
  The auditor also flagged `parse_searxng_html` compiling 2–5 regexes (around
  lines 208-212, 297). Confirm all sites: `grep -n "Regex::new" src/tools/web_search_tool.rs`.

- `src/gateway/mod.rs:1083-1085` — `extract_tool_calls_from_history` compiles two
  regexes each call. Confirm: `grep -n "Regex::new" src/gateway/mod.rs`.

- **The pattern to mirror** — the agent loop uses module-level statics:
  `src/agent/loop_.rs:48` and `:418` (verified) use `LazyLock<Regex>`. Read them:
  `grep -n "LazyLock<Regex>\|static .*Regex" src/agent/loop_.rs`.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Web search tests | `cargo test web_search` | all pass |
| Gateway tests | `cargo test gateway` | all pass |

## Scope

**In scope**:
- `src/tools/web_search_tool.rs` — hoist the DuckDuckGo + SearXNG parser regexes.
- `src/gateway/mod.rs` — hoist the `extract_tool_calls_from_history` regexes.

**Out of scope** (do NOT touch):
- `src/agent/loop_.rs` (it is the reference).
- The parsing/matching logic — patterns and capture-group usage stay byte-for-
  byte identical; only *where the regex is compiled* changes.
- Any other `Regex::new` site not listed above.

## Git workflow

- Branch: `advisor/014-hoist-per-call-regex-statics`
- One commit; message e.g.
  `perf: compile web_search + gateway-history regexes once via LazyLock`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Hoist the web_search regexes

For each per-call `Regex::new(PATTERN)?` in `web_search_tool.rs`, define a
module-level static and reference it:
```rust
use std::sync::LazyLock;
static DDG_LINK_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r#"<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>([\s\S]*?)</a>"#).expect("valid regex")
});
```
`expect("valid regex")` on a compile-time-constant pattern is idiomatic for
`LazyLock<Regex>` (the pattern can't vary at runtime; matches how the agent loop
does it — verify). Replace `link_regex` usages with `&DDG_LINK_RE` (or `DDG_LINK_RE.captures_iter(...)`).
Do the same for the snippet regex and each SearXNG regex.

**Verify**: `grep -n "Regex::new" src/tools/web_search_tool.rs` → no matches
inside per-call fns (only inside `LazyLock::new` closures at module level);
`cargo build 2>&1 | tail -5` → compiles.

### Step 2: Hoist the gateway history regexes

Same transformation for the two regexes in `extract_tool_calls_from_history`
(`gateway/mod.rs:1083-1085`) — module-level `LazyLock<Regex>` statics referenced
in the function.

**Verify**: `grep -n "Regex::new" src/gateway/mod.rs` → the two per-call
compilations are gone (any remaining `Regex::new` is inside a `LazyLock::new`);
`cargo build 2>&1 | tail -5` → compiles.

### Step 3: Confirm behavior unchanged

The existing tests for web search parsing and gateway history extraction are the
regression guard. If none exist for a given parser, add one small test asserting
a known input yields the same parsed output (see Test plan).

**Verify**: `cargo test web_search` and `cargo test gateway` → all pass.

## Test plan

- Prefer relying on existing parser tests (`grep -n "#\[test\]" src/tools/web_search_tool.rs`).
  If `parse_duckduckgo_results` / `extract_tool_calls_from_history` have no unit
  test, add one each:
  - `parse_duckduckgo_extracts_links`: feed a small fixed HTML snippet with two
    result links; assert the parsed output contains both titles/URLs.
  - `extract_tool_calls_from_history_parses_known`: feed a known history string;
    assert the extracted tool calls match the pre-change output.
  These lock the behavior so the hoist is provably a no-op.
- Verification: `cargo test web_search` and `cargo test gateway` → all pass,
  including any new parser tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `grep -n "Regex::new" src/tools/web_search_tool.rs src/gateway/mod.rs`
      shows every remaining `Regex::new` is inside a module-level `LazyLock::new`
      closure (none inside per-call functions)
- [ ] `cargo test web_search` and `cargo test gateway` pass
- [ ] Only the two in-scope files modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- A regex pattern turns out to be built from a runtime value (not a constant
  literal) — then it cannot be a static; leave that one and report it.
- Hoisting changes a capture-group index or the parser output (a test fails) —
  revert that site and report; the change must be behavior-preserving.

## Maintenance notes

- Any new HTML/text parser should compile its regexes as `LazyLock<Regex>`
  statics, never per call. Consider a clippy lint or review-checklist note.
- Reviewer should diff the regex pattern strings before/after to confirm they are
  identical (a stray character during the move would silently change matching).
