# Plan 015: Extract a shared rerank HTTP-transport helper (deduplicate cohere/vllm/llm POST-and-parse)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/kb/rerank/ src/kb/embed/`
> If any changed since this plan was written, compare the "Current state"
> excerpts against the live code; on a mismatch, treat it as a STOP condition.
>
> **Feature note**: KB code is behind `--features kb`; all build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P3
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: tech-debt
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

The three rerank backends (`cohere`, `vllm`, `llm`) each independently implement
the same POST-and-parse transport: build a `serde_json` body, `post().json().send()`,
check status, map error, `resp.json()`. Three copies means timeout handling,
error mapping, and header logic drift independently and must be fixed in three
places. The embed backends already avoid this with a shared `embed_via_http`
helper reused across backends — rerank should follow the same pattern. The
response *schemas* genuinely differ (Cohere vs vLLM vs LLM-as-judge), so only the
transport layer is shared, not the whole method.

## Current state

- `src/kb/rerank/cohere.rs:97-127` — the shape (verified):
  ```rust
  let body = serde_json::json!({ "model": &self.model, "query": query, "documents": documents, "top_n": final_k });
  let resp = self.http.post(&self.endpoint).bearer_auth(&self.api_key).json(&body).send().await?;
  let status = resp.status();
  if !status.is_success() {
      let text = resp.text().await.unwrap_or_default();
      return Err(KbError::ChatApi { status: status.as_u16(), body: truncate(&text, 300) });
  }
  let parsed: CohereResponse = resp.json().await?;
  ```
  `self.http` is a stored `reqwest::Client` (reuse is already fine — this plan is
  about the duplicated code shape, NOT client reuse).

- `src/kb/rerank/vllm.rs` (~lines 91-110) and `src/kb/rerank/llm.rs` (~lines
  97-121) repeat the same status-check/error-map/`resp.json()` structure with a
  different body and a different `*Response` struct. Read all three:
  `grep -n "post(\|send()\|resp.json\|is_success\|KbError" src/kb/rerank/cohere.rs src/kb/rerank/vllm.rs src/kb/rerank/llm.rs`.

- **The pattern to mirror** — `src/kb/embed/openrouter.rs:120` (verified):
  ```rust
  pub(in crate::kb::embed) async fn embed_via_http(http: &Client, url: &str, api_key: Option<&str>, body: &Value, expected_dim: usize) -> KbResult<Vec<Vec<f32>>> { ... }
  ```
  It centralizes POST + retry + status handling and is reused by `tei.rs`. Read
  it fully to match the retry/error conventions.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint (kb) | `cargo clippy --features kb --all-targets -- -D warnings` | exit 0 |
| Rerank tests | `cargo test --features kb rerank` | all pass |

## Scope

**In scope**:
- A new shared helper in `src/kb/rerank/` (e.g. add to `src/kb/rerank/mod.rs` a
  `pub(in crate::kb::rerank) async fn post_json_rerank(...)` — mirror embed's
  visibility scoping).
- `src/kb/rerank/cohere.rs`, `vllm.rs`, `llm.rs` — call the helper for transport,
  keep their own body-building and response-schema mapping.

**Out of scope** (do NOT touch):
- `src/kb/embed/` (it is the reference).
- The response schema structs (`CohereResponse`, the vLLM/LLM equivalents) — the
  divergence is real; keep per-backend parsing.
- The rerank scoring/ranking logic after parsing.
- Client construction (each already stores `self.http`).

## Git workflow

- Branch: `advisor/015-rerank-http-transport-helper`
- Commit per logical unit (helper, then each backend); messages e.g.
  `refactor(kb): share rerank HTTP transport helper across cohere/vllm/llm`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Add the shared transport helper

Add a helper that does POST + status-check + error-map and returns the raw
response for the caller to deserialize into its own schema. Two viable shapes —
pick the one that matches how `embed_via_http` is structured:

**Shape A (return deserialized via generic):**
```rust
pub(in crate::kb::rerank) async fn post_json_rerank<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client, endpoint: &str, api_key: Option<&str>, body: &serde_json::Value,
) -> KbResult<T> {
    let mut req = http.post(endpoint).json(body);
    if let Some(k) = api_key { req = req.bearer_auth(k); }
    let resp = req.send().await.map_err(KbError::Http)?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(KbError::ChatApi { status: status.as_u16(), body: truncate(&text, 300) });
    }
    resp.json::<T>().await.map_err(KbError::Http)   // adjust error variant to match embed's
}
```
Match the exact `KbError` variants and `truncate` import used by the existing
code (read them from `cohere.rs`). If `embed_via_http` includes retry, mirror
that here so behavior is consistent; if not, keep it simple.

**Verify**: `cargo build --features kb 2>&1 | tail -5` → compiles.

### Step 2: Convert the three backends

In each of `cohere.rs`, `vllm.rs`, `llm.rs`, replace the inline
`post().json().send()` + status-check + `resp.json()` block with:
```rust
let parsed: CohereResponse = post_json_rerank(&self.http, &self.endpoint, Some(&self.api_key), &body).await?;
```
(`api_key` is `None` for backends that don't authenticate — check each). Keep the
body construction and the post-parse ranking mapping unchanged in each file.

**Verify**: `grep -n "resp.json\|is_success" src/kb/rerank/cohere.rs src/kb/rerank/vllm.rs src/kb/rerank/llm.rs`
→ no matches (transport moved to the helper); `cargo build --features kb 2>&1 | tail -5` → compiles.

## Test plan

- Prefer existing rerank tests. If the backends have tests using `wiremock`/
  `mockito` (dev-deps), they cover the conversion — run them.
- Add one helper-level test if none exists:
  - `post_json_rerank_maps_error_status`: point at a `wiremock` server returning
    500; assert `KbError::ChatApi` with the right status.
  - `post_json_rerank_parses_ok`: `wiremock` returns a small JSON matching a test
    struct; assert it deserializes.
  - Model after existing KB HTTP tests: `grep -rln "wiremock\|MockServer" src/kb/ tests/kb/`.
- Verification: `cargo test --features kb rerank` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --features kb --all-targets -- -D warnings` exits 0
- [ ] `grep -rn "is_success\|resp.json" src/kb/rerank/{cohere,vllm,llm}.rs` shows
      transport is no longer inline in the three backends
- [ ] `cargo test --features kb rerank` passes
- [ ] Only in-scope files modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The three backends' error handling / retry behavior differ enough that a single
  helper would change one backend's behavior — report the divergence; it may be
  fine to share only two, or to parameterize retry.
- `KbError` variants or `truncate` are not where the excerpt implies (drift).

## Maintenance notes

- If a fourth rerank backend is added, it should use `post_json_rerank` from day
  one — note this near the helper.
- Reviewer should confirm each backend's *response schema* mapping is unchanged;
  only transport moved. A shared helper must not homogenize genuinely different
  response shapes.
