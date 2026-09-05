# Plan 087: Scope the KB 32 MiB body limit to the ingest route

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 2ca7e59..HEAD -- src/kb/axi/api.rs`
> If any of these changed since this plan was written, compare the "Current
> state" excerpts against the live code; on a mismatch, treat it as a STOP
> condition.
>
> **Feature note**: KB code is behind `--features kb`. All build/test commands
> below MUST pass `--features kb`.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: MED
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `2ca7e59`, 2026-08-10

## Why this matters

The KB router turns off the gateway-wide request-body limit for **every** route
it owns, not just the upload one. The gateway sets that limit to 64 KiB
precisely so an unauthenticated caller cannot make the process buffer large
bodies.

`src/kb/axi/api.rs:115-116`:

```rust
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(KB_UPLOAD_MAX_BYTES))   // 32 MiB
```

These layers apply to all 12 routes registered above them (`api.rs:88-114`),
including `POST /api/v1/kb/search`, `POST /api/v1/kb/groups` and
`POST /api/v1/kb/re-embed`.

Authentication does not help. `check_auth` runs inside each handler body — e.g.
`search` at `api.rs:754` — but axum runs the `Json<T>` extractor first, which
buffers the request body before the handler is entered. So an unauthenticated
caller can push up to 32 MiB per request into the process and only then receive
a 401.

The router's own doc comment (`api.rs:83-85`) claims this is fine because
"axum drops the body if it exceeds Content-Length before any handler is
called". That reasoning covers over-long bodies, not bodies that are within the
raised limit and therefore accepted.

Under CLAUDE.md §5 `src/gateway/**` is a high-risk path, and §3.6 puts exposure
surfaces under deny-by-default.

## Current state (verified at 2ca7e59)

- `KB_UPLOAD_MAX_BYTES = 32 * 1024 * 1024` — `api.rs:57`
- Router built in `pub fn router()` — `api.rs:86-121`
- The only handler that needs a large body is `ingest` (`api.rs:1206`), which
  takes `Multipart`.
- The gateway merges this router unconditionally under `#[cfg(feature = "kb")]`
  — `src/gateway/mod.rs:848-849`.
- No test covers body limits on KB routes.

## Scope

**In scope**: apply the raised limit only to the ingest route; leave every
other KB route on the gateway default.

**Out of scope**: moving `check_auth` into middleware. That is a broader
gateway change and is not required to close this — a smaller body cap is the
fix.

## Git workflow

```bash
git switch -c fix/kb-body-limit-scoped-to-ingest
```

## Steps

### Step 1: Layer the ingest METHOD, not a sub-router

The obvious shape — a separate `Router` for the upload path — does not work
cleanly here: `POST` and `GET` share `/api/v1/kb/documents`, and axum panics on
overlapping route paths across merged routers. Putting both on the sub-router
would leave `GET /documents` on the raised limit, which is the same defect in a
smaller costume.

Layer the `MethodRouter` instead, so only the `POST` arm is exempt. Verified
available in axum 0.8: `MethodRouter::layer`
(`axum-0.8/src/routing/method_routing.rs:967`) and `MethodRouter::merge`
(`:1091`).

```rust
pub fn router() -> Router<AppState> {
    // Ingest accepts multipart uploads and needs a far larger body than the
    // gateway default. Keep that exemption on the POST arm ALONE — the raised
    // limit applies before `check_auth` (axum runs the `Json`/`Multipart`
    // extractor before the handler body, and `check_auth` lives in the handler
    // body), so a wider blast radius lets an unauthenticated caller make the
    // process buffer 32 MiB per request.
    let ingest_route = post(ingest)
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(KB_UPLOAD_MAX_BYTES))
        .merge(get(list));

    Router::new()
        .route("/api/v1/kb/search", post(search))
        .route("/api/v1/kb/documents", ingest_route)
        .route("/api/v1/kb/documents/{id}", get(get_doc).delete(delete_doc))
        // ... every other existing route, unchanged ...
        .route("/api/v1/kb/graph", get(get_graph))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(KB_REQUEST_TIMEOUT_SECS),
        ))
}
```

Keep the `TimeoutLayer` on the outer router so it still covers every route
including ingest.

If the `MethodRouter` layering does not typecheck against the concrete
`AppState` (the error types must line up), fall back to a sub-router carrying
**only** `post(ingest)` and move `get(list)` to a distinct path — but that
changes the public API, so raise it before doing it rather than deciding
silently.

**Verify**: `cargo build --features kb` succeeds; the route list is unchanged
(`grep -c '"/api/v1/kb' src/kb/axi/api.rs` still reports 12 route strings).

### Step 2: Cover both directions with tests

In `tests/kb/api_test.rs`, add two tests using the existing `start_harness`:

1. A ~1 MiB JSON body to `POST /api/v1/kb/search` must be rejected with
   `413 Payload Too Large`, not processed.
2. A ~1 MiB body to `GET /api/v1/kb/documents` must also be rejected — this is
   the arm the sub-router shape would have missed.
3. A small multipart upload to `POST /api/v1/kb/documents` must still be
   accepted (it may fail later for other reasons — assert the status is NOT
   413).

Test 3 is the control. Without it, tests 1-2 pass trivially if a mistake
shrinks every limit including ingest.

**Verify**: both tests pass; test 1 fails if you revert Step 1.

## Test plan

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --features kb -- -D warnings
cargo test --features kb --test kb api_test
```

Manual:

```bash
head -c 1000000 /dev/zero | tr '\0' 'a' > /tmp/big.txt
python3 -c "import json,sys;print(json.dumps({'query':open('/tmp/big.txt').read()}))" > /tmp/big.json
curl -s -o /dev/null -w '%{http_code}\n' -X POST localhost:9393/api/v1/kb/search \
  -H 'content-type: application/json' --data @/tmp/big.json
# expect 413
```

## Done criteria

- Only `/api/v1/kb/documents` carries the raised limit.
- An oversized body on `/api/v1/kb/search` is refused with 413.
- A normal upload still succeeds.

## STOP conditions

- The route set changed since `2ca7e59` — re-derive the split rather than
  pasting the block above.
- Removing `DefaultBodyLimit::disable()` from the outer router breaks ingest in
  the test — that means the gateway default is being applied to the merged
  sub-router; investigate layer ordering before proceeding.

## Maintenance notes

Any new KB route defaults to the gateway limit now. A future route that needs a
larger body must join the `upload` sub-router explicitly — that is the point.
