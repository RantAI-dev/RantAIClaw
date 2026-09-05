# Plan 289: Stop the GLM text parser turning a bare URL line into a shell `curl`

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat 4b8f61e..HEAD -- src/agent/loop_.rs`
> Mismatch against the excerpt below = STOP.

## Status

- **Priority**: P1 (ledger W1-2)
- **Effort**: S
- **Risk**: LOW–MED (GLM users who rely on the bare-URL shorthand lose it, by design)
- **Category**: security
- **Planned at**: commit `4b8f61e` (v0.28.0-alpha), 2026-09-04

## Why this matters

The parser has an explicit, correct security rule directly above it: never extract tool calls
from unstructured response text, because content the model echoes from an email, a file or a
web page could mimic one. The GLM fallback then breaks that rule — any line that is just a
URL becomes a `shell` call running `curl` against it.

That turns "the model quoted a link it read somewhere" into an outbound request from the
operator's machine. Under Full autonomy, or with `curl` on the command allowlist, it executes
with no prompt. The alias table also maps `web_search` and `browser` to `shell`, so a
search-shaped line becomes a shell command too.

## Current state (verified at `4b8f61e`)

```rust
// src/agent/loop_.rs:727-733
// Plain URL
if let Some(command) = build_curl_command(line) {
    calls.push((
        "shell".to_string(),
        serde_json::json!({"command": command}),
```

The alias table that widens this further:

```rust
// src/agent/loop_.rs (map_glm_tool_alias)
"browser_open" | "browser" | "web_search" | "shell" | "bash" => "shell",
```

The rule it contradicts is the `// SECURITY:` comment block in the same file, which states
tool calls must be explicitly wrapped in a structured form.

## Steps

1. **Delete the plain-URL arm** at `:727`. A line that is only a URL is prose, not a tool
   call.
   **Verify**: `rg -n '// Plain URL' src/agent/loop_.rs` returns nothing.

2. **Narrow the alias table.** `shell`/`bash` → `shell` is defensible (the model named a
   shell). `web_search` and `browser`/`browser_open` → `shell` is not: it silently converts a
   read-only intent into command execution. Map them to their real tools
   (`web_search_tool`, the browser tool) or drop them so the call is reported as unknown.
   **Verify**: read the tool registry for the exact registered names before mapping — the
   audit found `web_search` vs `web_search_tool` drift already.

3. **Gate the remaining GLM line grammar behind the provider that needs it**, rather than
   running it for every response that lacks a structured call. The parser currently runs even
   for native providers that simply returned no tool calls.
   **Verify**: a response from a non-GLM provider containing `shell/command>ls` produces no
   tool call.

4. **Negative tests.** At minimum: a bare `https://…` line yields zero tool calls; a
   model reply quoting a URL from fetched content yields zero tool calls; the legitimate
   `shell/command>ls` form still works for GLM.
   **Verify**: `cargo test --lib agent` passes; test one fails if step 1 is reverted.

## Done criteria

- `cargo fmt --all -- --check` clean.
- `cargo clippy -p rantaiclaw --lib -- -D clippy::correctness` clean.
- `cargo test --lib agent` passes with the new negative tests.
- No path from unstructured response text to a `shell` tool call remains.

## STOP conditions

- Removing the alias breaks an existing passing test that asserts the URL→curl behaviour →
  STOP and report: that test encodes the bug as the contract, and deleting it is a decision
  worth stating explicitly in the PR rather than doing quietly.

## Test plan

Three tests beside the existing GLM parser tests in `loop_.rs`.

## Maintenance note

The `// SECURITY:` block in this file states the rule. Any future fallback parser must be
checked against it — this arm was added underneath it.

## Rollback

One commit, one file plus tests.
