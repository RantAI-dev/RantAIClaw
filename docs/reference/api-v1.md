# API v1 Reference

> **Status: stability obligation.** This page documents the `/api/v1/*` HTTP
> surface as it exists in the code (`src/gateway/api_v1.rs`) at the time of
> writing. Publishing this contract means callers outside the bundled console
> may start depending on it — treat shape/behavior changes to any route below
> as a compatibility change, not an internal refactor.
>
> **Not yet test-enforced.** This doc is grounded in the handler source but
> has no automated contract test today. Plan 013 (`spawn_test_gateway()` test
> harness) has not landed in this branch; once it does, a follow-up should add
> `tests/api_v1.rs` asserting auth + response-shape for a representative
> subset of routes below, so this contract stops being aspirational. An
> OpenAPI spec generated from the router is a further, separate follow-up
> (deferred) — it would let the contract test and this doc derive from one
> source instead of two hand-maintained descriptions.
>
> English-only: this repository does not ship translated docs.

`/api/v1` is the control-plane HTTP API — the same backend code paths the
CLI/TUI hit via slash commands, exposed over HTTP so a web console or a
third-party integration can drive sessions, memory, skills, providers, and
chat without shelling out to the binary. It is mounted alongside the
webhook/gateway server (`src/gateway/mod.rs`) and shares that server's body
size limit and request timeout.

## Auth model

Every route below requires `Authorization: Bearer <token>` **except**
`GET /api/v1/version` and `GET /api/v1/auth/info`, which are intentionally
public (no login-required check, so a fresh console can render before the
user authenticates).

- Auth is enforced per-handler via a `check_auth` guard, not by a blanket
  middleware layer.
- Gating is controlled by `gateway.require_pairing` in config. When it is
  `false` (the local-dev default), **every** route — including the ones
  marked "bearer-gated" below — accepts requests with no token at all. When
  `true`, a gated route without a valid `Authorization: Bearer <token>`
  header returns `401`.
- Obtain a token via `POST /pair` (outside `/api/v1`, not documented here).
- `GET /api/v1/providers` is gated as of a companion fix
  (branch `advisor/020-providers-api-auth-gate`) that closed a gap where this
  one route shipped without the `check_auth` call every sibling route has.
  If you are reading this against an older checkout, verify that fix has
  landed before relying on this route being auth-gated.

A `401` response body looks like:

```json
{
  "error": "unauthorized",
  "detail": "Pair via POST /pair, then send `Authorization: Bearer <token>`."
}
```

## Error shape

Non-2xx responses share one shape:

```json
{
  "error": "bad_request",
  "detail": "optional human-readable detail, omitted when not set"
}
```

`error` is one of `unauthorized` (401), `bad_request` (400), `not_found`
(404), `internal_error` (500). `detail` is present when the handler has more
to say (it is always present for `internal_error`, which carries the
sanitized error text).

## Example base URL

```text
http://127.0.0.1:9393
```

(`9393` is the config schema default for `gateway.port`; your instance may be
configured differently.) Examples below use neutral placeholders — no real
tokens, session ids, or paths.

---

## Meta

### GET /api/v1/version

- **Auth**: public.
- **Request**: none.
- **Response** `200`:
  ```json
  {
    "version": "0.7.x",
    "name": "rantaiclaw",
    "config_fingerprint": "..."
  }
  ```
  `config_fingerprint` changes whenever the on-disk config is hot-reloaded;
  useful for a console to detect it should refetch `/status`.

### GET /api/v1/auth/info

- **Auth**: public.
- **Request**: none.
- **Response** `200`:
  ```json
  { "login_required": false }
  ```
  `login_required` reflects whether a username+password login is configured
  (`gateway.login.password_hash` set). The username itself is deliberately
  never returned here (avoids enumeration) — the console collects it from the
  user on the login form.

### GET /api/v1/status

- **Auth**: bearer-gated.
- **Request**: none.
- **Response** `200`:
  ```json
  {
    "version": "0.7.x",
    "provider": "openrouter",
    "model": "some-model-id",
    "memory_backend": "...",
    "autonomy": "Supervised",
    "workspace_dir": "/path/to/workspace",
    "paired": true,
    "runtime": {
      "uptime_seconds": 123,
      "components": {
        "<component-name>": {
          "status": "ok",
          "updated_at": "...",
          "last_ok": "...",
          "last_error": null,
          "restart_count": 0
        }
      }
    }
  }
  ```
  `runtime` is `crate::health::snapshot_json()` — a process health snapshot
  keyed by component name; the shape above is confirmed from its own test
  assertions, not enumerated exhaustively in `api_v1.rs`.
- **Status codes**: `200`, `401`.

### GET /api/v1/doctor

- **Auth**: bearer-gated.
- **Request**: none.
- **Response** `200`:
  ```json
  {
    "results": [
      {
        "name": "...",
        "category": "...",
        "severity": "Ok",
        "message": "...",
        "hint": null,
        "duration_ms": 12
      }
    ]
  }
  ```
  `severity` is one of `"Ok"`, `"Warn"`, `"Fail"`, `"Info"` (Rust `Debug`
  formatting of `doctor::Severity`, PascalCase — not the lowercase
  `as_str()` form the CLI uses elsewhere). This endpoint always runs in
  **offline/brief mode** (`offline: true`) — no live network probes, so it
  is safe to poll from a console without side effects.
- **Status codes**: `200`, `401`.

---

## Agent chat & approvals

### POST /api/v1/agent/chat

- **Auth**: bearer-gated.
- **Request body**:
  ```json
  {
    "message": "required, non-empty string",
    "model": "optional string override",
    "provider": "optional string override",
    "temperature": 0.7,
    "session_id": "optional — continue this session; absent/empty starts a new one"
  }
  ```
- **Streaming vs sync**: this single route serves both modes from the same
  handler. It streams via Server-Sent Events when either the request has
  `Accept: text/event-stream`, or the query string has
  `?stream=1|true|yes|on`; otherwise it returns one synchronous JSON body.
  Event types, SSE framing, and the `ApprovalRequest`/`ToolCallStart`/
  `CompactionStart` event payloads are documented in
  [api-v1-streaming.md](api-v1-streaming.md) rather than duplicated here.
- **Sync response** `200`:
  ```json
  {
    "text": "assistant reply",
    "model": "resolved-model-id",
    "provider": "resolved-provider-id",
    "duration_ms": 1234,
    "session_id": "the session this turn was persisted to"
  }
  ```
  A completed, non-empty turn is persisted to `sessions.db` with
  `source = "api"`; a failed or empty-answer turn is not persisted (no
  session is created/appended).
- **Status codes**: `200`, `400` (empty `message`), `401`,
  `500` (provider/agent error — the body is sanitized of secret-looking
  tokens before being returned).

### POST /api/v1/approvals/{id}

Resolves an in-browser tool-approval modal raised mid-turn by a streaming
`agent/chat` call (see the `approval_request` SSE event in
[api-v1-streaming.md](api-v1-streaming.md)). Only relevant when tool-gating
is on, i.e. `channels_config.autonomous_tools` is not set.

- **Auth**: bearer-gated.
- **Path param**: `id` — the approval id carried by the `approval_request`
  SSE event.
- **Request body**:
  ```json
  { "approve": true }
  ```
  `true` approves the tool call once; `false` denies it.
- **Response** `200`:
  ```json
  { "resolved": true, "id": "...", "approved": true }
  ```
- **Status codes**: `200`, `404` (no pending approval with that id — already
  resolved, timed out, or unknown), `401`.

---

## Sessions

All session-lookup-by-id routes (`GET/DELETE /sessions/{id}`,
`PUT /sessions/{id}/title`) resolve `id` as a **prefix match** against known
session ids, not an exact match: `0` matches means `404`, exactly `1` match
resolves normally, `2+` matches return `400` ("ambiguous").

### GET /api/v1/sessions

- **Auth**: bearer-gated.
- **Query params**: `limit` — optional, default `50`, capped at `500`.
- **Response** `200`:
  ```json
  {
    "sessions": [
      {
        "id": "...",
        "title": "may be null",
        "model": "...",
        "started_at": 1700000000,
        "message_count": 4
      }
    ],
    "count": 1
  }
  ```
  `started_at` is a Unix epoch second integer (`i64`); `message_count` counts
  stored messages, `title` is nullable (unset until the store derives one).
- **Status codes**: `200`, `401`.

### POST /api/v1/sessions/search

- **Auth**: bearer-gated.
- **Request body**:
  ```json
  { "query": "required, non-empty", "limit": 20 }
  ```
  `limit` optional, default `20`, capped at `200`.
- **Response** `200`:
  ```json
  {
    "results": [
      {
        "session_id": "...",
        "session_title": "may be null",
        "role": "user",
        "content": "...",
        "timestamp": 1700000000,
        "rank": 0.83
      }
    ],
    "count": 1
  }
  ```
  Full-text search (SQLite FTS5) over stored messages; `rank` is a floating
  point relevance score (`f64`), lower/higher-is-better is FTS5's own
  convention, not redefined here.
- **Status codes**: `200`, `400` (empty `query`), `401`.

### GET /api/v1/sessions/{id}

- **Auth**: bearer-gated.
- **Path param**: `id` (prefix match, see note above).
- **Response** `200`:
  ```json
  {
    "id": "...",
    "title": "may be null",
    "model": "...",
    "started_at": 1700000000,
    "messages": [
      { "role": "user", "content": "...", "timestamp": 1700000000 }
    ]
  }
  ```
- **Status codes**: `200`, `404` (no match), `400` (ambiguous prefix), `401`.

### DELETE /api/v1/sessions/{id}

- **Auth**: bearer-gated.
- **Path param**: `id` (prefix match, see note above).
- **Response** `200`:
  ```json
  { "deleted": true, "id": "..." }
  ```
- **Status codes**: `200`, `404`, `400` (ambiguous), `401`.

### PUT /api/v1/sessions/{id}/title

- **Auth**: bearer-gated.
- **Path param**: `id` (prefix match, see note above).
- **Request body**:
  ```json
  { "title": "new title" }
  ```
- **Response** `200`:
  ```json
  { "id": "...", "title": "new title" }
  ```
- **Status codes**: `200`, `404`, `400` (ambiguous), `401`.

---

## Insights

### GET /api/v1/insights

- **Auth**: bearer-gated.
- **Request**: none.
- **Response** `200`:
  ```json
  {
    "total_sessions": 10,
    "total_messages": 42,
    "avg_messages_per_session": 4.2,
    "latest_session_id": "may be null",
    "latest_session_started_at": 1700000000
  }
  ```
  Computed by scanning up to the 10,000 most recent sessions on every call —
  no caching. `avg_messages_per_session` is `0.0` when there are no sessions.
- **Status codes**: `200`, `401`.

---

## Skills

### GET /api/v1/skills

- **Auth**: bearer-gated.
- **Request**: none.
- **Response** `200`:
  ```json
  {
    "skills": [
      {
        "name": "...",
        "version": "...",
        "description": "...",
        "tags": ["..."],
        "tools": ["tool_name_a", "tool_name_b"]
      }
    ],
    "count": 1
  }
  ```
  `tools` here is just the tool **names** the skill exposes.
- **Status codes**: `200`, `401`.

### GET /api/v1/skills/{name}

- **Auth**: bearer-gated.
- **Path param**: `name` — matched case-insensitively.
- **Response** `200`:
  ```json
  {
    "name": "...",
    "version": "...",
    "description": "...",
    "tags": ["..."],
    "tools": [{ "name": "...", "description": "..." }]
  }
  ```
  Unlike the list endpoint, `tools` here includes each tool's description.
- **Status codes**: `200`, `404` (no skill with that name), `401`.

---

## Memory

### GET /api/v1/memory

- **Auth**: bearer-gated.
- **Query params**: `limit` — optional, default `50`, capped at `500`.
- **Response** `200`:
  ```json
  {
    "entries": [
      {
        "key": "...",
        "category": "core",
        "content": "...",
        "timestamp": "...",
        "session_id": "may be null"
      }
    ],
    "count": 1
  }
  ```
  `category` is one of `"core"`, `"daily"`, `"conversation"`, or a
  custom category string. **`timestamp` here is a string**, not the Unix
  epoch integer sessions/messages use elsewhere in this API — the memory
  backend and the sessions store encode time differently; this is a real,
  current inconsistency across resource groups worth knowing about if you're
  writing a client that parses both.
  The handler fetches the full entry list from the backend and truncates to
  `limit` in the response — `limit` bounds the response size, not the
  underlying query.
- **Status codes**: `200`, `401`.

### GET /api/v1/memory/stats

- **Auth**: bearer-gated.
- **Request**: none.
- **Response** `200`:
  ```json
  { "backend": "...", "total_entries": 100, "healthy": true }
  ```
- **Status codes**: `200`, `401`.

---

## Personality

### GET /api/v1/personality

- **Auth**: bearer-gated.
- **Request**: none.
- **Response** `200`, persona configured:
  ```json
  {
    "profile": "default",
    "preset": "concise_pro",
    "name": "...",
    "timezone": "...",
    "role": "...",
    "tone": "...",
    "avoid": "may be null",
    "always_on_kbs": ["..."]
  }
  ```
- **Response** `200`, no persona configured yet:
  ```json
  { "profile": "default", "preset": null, "configured": false }
  ```
  Note the two response shapes for the same `200` differ by whether
  `configured` is present — a client must branch on that field rather than
  assuming a fixed shape.
- **Status codes**: `200`, `401`.

### PUT /api/v1/personality

Partial update — every field is optional; only supplied fields overwrite the
current persona (absent fields are left as-is). If no persona exists yet, one
is created first.

- **Auth**: bearer-gated.
- **Request body** (all fields optional):
  ```json
  {
    "preset": "concise_pro",
    "name": "...",
    "role": "...",
    "tone": "...",
    "avoid": "",
    "always_on_kbs": ["..."]
  }
  ```
  - `preset`, when supplied, must be one of: `default`, `concise_pro`,
    `friendly_companion`, `research_analyst`, `executive_assistant` — any
    other value is a `400`.
  - `avoid`: an empty string **clears** the "things to avoid" block; a
    non-empty string sets it; the field being absent leaves it unchanged
    (three distinct behaviors for one field — not "empty means unset").
- **Response** `200`: the persisted persona in the same shape as the
  "persona configured" branch of `GET /api/v1/personality` above (minus
  `profile`/`timezone`).
- **Status codes**: `200`, `400` (unknown `preset`), `401`.

---

## Channels

### GET /api/v1/channels

- **Auth**: bearer-gated.
- **Request**: none.
- **Response** `200`:
  ```json
  { "configured": ["telegram", "whatsapp"], "count": 2 }
  ```
  `configured` lists which of a **fixed, hardcoded set of seven** channel
  types (`telegram`, `discord`, `slack`, `mattermost`, `imessage`, `signal`,
  `whatsapp`) have a config section present. This is read-only — it does not
  report health/connection status, only "configured or not."
  **Known gap**: `config::schema::ChannelsConfig` also has `matrix`, `linq`,
  `irc`, and `lark` sub-configs; this endpoint does not check them, so a
  Matrix/IRC/Lark/Linq channel that is configured will not appear in
  `configured` even though it is active. Confirmed by comparing the checks
  in `channels_list` (`src/gateway/api_v1.rs`) against the full field list in
  `ChannelsConfig` (`src/config/schema.rs`) — not something this doc invents.
- **Status codes**: `200`, `401`.

---

## Providers

### GET /api/v1/providers

- **Auth**: bearer-gated (as of the companion fix noted under
  [Auth model](#auth-model) above — verify it has landed if you are reading
  this against an older checkout).
- **Request**: none.
- **Response** `200`:
  ```json
  {
    "providers": [
      {
        "id": "openrouter",
        "display_name": "OpenRouter",
        "aliases": [],
        "local": false
      }
    ],
    "count": 1
  }
  ```
  This is a static, compiled-in catalog (`crate::providers::list_providers`)
  — the same for every install regardless of configured API keys; it does
  not reflect which providers are actually usable in this instance.
- **Status codes**: `200`, `401`.

### GET /api/v1/providers/{id}/models

- **Auth**: bearer-gated.
- **Path param**: `id` — a provider name. An unrecognized `id` does **not**
  404 — it falls through to an empty/curated catalog with `source: "curated"`
  and an empty `models` list, still `200`.
- **Response** `200`:
  ```json
  {
    "provider": "...",
    "models": ["model-id-a", "model-id-b"],
    "default": "model-id-a",
    "source": "cache",
    "age_secs": 120,
    "count": 2
  }
  ```
  `source` is `"cache"` (from the on-disk `models_cache.json`, unioned with
  the curated fallback list) or `"curated"` (no cache entry). `age_secs` is
  `null` when `source` is `"curated"`. This never makes a network call — see
  the refresh endpoint below for that.
- **Status codes**: `200`, `401`.

### POST /api/v1/providers/{id}/models/refresh

- **Auth**: bearer-gated.
- **Path param**: `id` — a provider name.
- **Request**: no body.
- **Behavior**: fetches the provider's live model list (network I/O, run on
  a blocking thread) and writes it to `models_cache.json`, then returns the
  refreshed catalog. A failed live fetch (e.g. missing API key) is
  **best-effort and non-fatal** — it logs a warning and still returns the
  existing (cache/curated) catalog with `refreshed: false` rather than a
  `500`; only a panicked background task produces a `500`.
- **Response** `200`:
  ```json
  {
    "provider": "...",
    "models": ["model-id-a"],
    "default": "model-id-a",
    "source": "cache",
    "age_secs": 3,
    "count": 1,
    "refreshed": true,
    "detail": "present only when refreshed is false — the fetch error"
  }
  ```
- **Status codes**: `200`, `401`, `500` (only if the refresh task itself
  panics — not for an ordinary fetch failure).

---

## Maintenance

- Every new `/api/v1` route must be added to this reference (and, once plan
  013 lands, get a contract test) as part of the same change that adds it —
  do not let this page drift from `src/gateway/api_v1.rs`.
- If you find a route here whose actual behavior no longer matches what's
  written, that is a documentation bug (or, if the *code* changed
  unintentionally, a regression) — fix the mismatch rather than working
  around it silently.
