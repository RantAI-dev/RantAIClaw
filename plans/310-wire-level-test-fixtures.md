# Plan 310: Test the wire, not the helpers around it

> **Executor instructions**: follow step by step; verify each step; a STOP-condition means
> stop and report. Update the ledger row in `plans/280-production-readiness-handoff.md`.
>
> **Drift check (run first)**: `git diff --stat bf77d26..HEAD -- src/providers/ src/kb/ tests/`

## Status

- **Priority**: P2 (ledger W2-5) · **Effort**: M · **Risk**: LOW
- **Category**: tests
- **Planned at**: commit `bf77d26`, 2026-09-05

## Why this matters

Three critical paths have high test counts and no coverage of the thing that actually breaks.

**Providers**: the only mocked-HTTP test exercises `chat_with_tools`, which has no production
caller. `RigProvider::chat`/`chat_stream` — the default path for Anthropic, OpenAI and Gemini —
has no response-shape test at all. The regressions this repo has actually shipped are exactly
this class: a base URL that 404s, tool ids that produce a 400, a UTF-8 split mid-chunk.

**KB**: every test that drives real ingest → embed → store → retrieve is `#[ignore]`d because
it wants a live embedder. The parity test too. So the feature the KB exists for is verified by
hand only, while `wiremock` is already a dev-dependency used elsewhere.

**claw-ui**: `api.ts` covers roughly 55 endpoints and has no contract test; thirteen component
suites mock it away. A path or method typo ships green.

## Steps

1. **One fixture per provider wire shape**, not per provider: an OpenAI-compatible server, an
   Anthropic-shaped server, and Ollama's NDJSON. Assert the request body the client sends
   (including tool definitions) and the parsed `ChatResponse` (including tool calls and a
   split-mid-codepoint stream).
   **Verify**: mutate the base-URL construction — the test must fail. That is the regression
   the project has shipped before.
2. **One un-ignored KB path test.** A wiremock embeddings endpoint plus a real `SqliteStore`:
   ingest a document, retrieve it, assert the seeded chunk comes back. Keep the live-key tests
   ignored; this one must run in CI.
3. **A table-driven contract test for `api.ts`** in claw-ui: for each exported call, assert
   URL, method and body against a stubbed `fetch`. Generate the table from the module rather
   than hand-listing, so a new endpoint is covered by construction.
4. **Do not chase coverage percentages.** The goal is that each of the three named regression
   classes has a test that fails when reintroduced.

## Done criteria

- `cargo test --lib providers` includes wire-shape tests for the three client families.
- The KB ingest→retrieve test runs in CI, un-ignored, without network access.
- `npx vitest run` includes the `api.ts` contract table.
- Each new test fails when its target behaviour is mutated.

## STOP conditions

- A provider's client cannot be pointed at a local server without changing production code →
  STOP and report; a seam may be worth adding, but that is its own change.

## Maintenance note

The pattern worth keeping: assert on what crosses the process boundary. Tests that stop at the
helper below it are what let all three of these gaps persist under a large test count.

## Rollback

Tests only; no production behaviour changes.
