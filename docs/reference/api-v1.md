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
    ],
    "skipped": ["provider.ping", "channels.auth", "mcp.startup"]
  }
  ```
  `severity` is one of `"Ok"`, `"Warn"`, `"Fail"`, `"Info"` (Rust `Debug`
  formatting of `doctor::Severity`, PascalCase — not the lowercase
  `as_str()` form the CLI uses elsewhere). This endpoint runs in
  **offline/brief mode**: it runs the seven non-live checks (config,
  provider-key presence, api-url, paths, allowlist, daemon registration,
  system deps) and makes no network calls. It is **not** fully side-effect-free
  — it spawns `systemctl`/`launchctl` to detect the daemon and writes a
  short-lived probe file in the workspace to verify writability (both moved off
  the async runtime). The three live checks (`provider.ping`, `channels.auth`,
  `mcp.startup`) are not run in brief mode; their names are returned in
  `skipped` so a client can say so rather than imply an all-green gateway.
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
    "session_id": "optional — continue this session; absent/empty starts a new one",
    "context": "optional — retrieved reference material for this turn only"
  }
  ```
  `temperature` must be finite and in `0.0`–`2.0` (else `400`). `context`, when
  present, is placed in the prompt as clearly-framed reference material for
  **this turn only** — it is not persisted with the user message and never
  enters replayed history, so retrieved documents do not compound across turns.
- **Choosing the session id**: `session_id` may name a session that does not
  exist yet. If it is UUID-shaped (`8-4-4-4-12` hex), the turn is persisted
  under exactly that id; anything else is ignored and the server mints one. This
  lets a client settle on an id *before* its first turn — the web console needs
  one at attachment-upload time, which happens before any session exists, so
  that the KB category and the session id are the same value. A supplied id that
  already exists continues that session, as before.
- **Streaming vs sync**: this single route serves both modes from the same
  handler. It streams via Server-Sent Events when either the request has
  `Accept: text/event-stream`, or the query string has
  `?stream=1|true|yes|on`; otherwise it returns one synchronous JSON body.
  The full event table (including `approval_request`, `memory_recalled`,
  `reload_complete`, and both `compaction_*` events) and the SSE framing are
  documented in [api-v1-streaming.md](api-v1-streaming.md) rather than
  duplicated here.
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
- **Query params**:
  - `limit` — optional, default `50`, capped at `500`. This is a **page size**,
    not a ceiling on what exists.
  - `offset` — optional, default `0`. Rows to skip, newest first. Without it the
    API could only ever return the newest 500 sessions and anything older was
    unreachable. Page with `offset += limit` until `offset + count >= total`.
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
    "count": 1,
    "offset": 0,
    "total": 1
  }
  ```
  `count` is the rows in *this* page; `total` is how many sessions exist, so a
  client can tell whether more pages remain. `started_at` is a Unix epoch second
  integer (`i64`); `message_count` counts stored messages, `title` is nullable
  (unset until the store derives one).
- **Status codes**: `200`, `401`.

### POST /api/v1/sessions/search

- **Auth**: bearer-gated.
- **Request body**:
  ```json
  { "query": "required, non-empty", "limit": 20 }
  ```
  `limit` optional, default `20`, capped at `200`. The query is matched
  literally — each whitespace token is treated as a quoted phrase, so FTS5
  operator characters (`"`, `*`, `(`, `NEAR`, `OR`) are searched as text rather
  than parsed as query syntax (which previously produced a `500`).
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
  Computed with aggregate SQL queries (`COUNT`/`SUM`), so the totals stay
  correct for any number of sessions. `avg_messages_per_session` is `0.0` when
  there are no sessions.
- **Status codes**: `200`, `401`.

---

## Skills

The mutating endpoints below (`install`, `enabled`, uninstall, and the three
authoring routes) are **owner-scoped mutations equivalent to the local CLI**
(`skills install`/`enable`/`disable`/`remove`, and `/skill new`/`/skill edit`
in the TUI) — they let the same pairing-authenticated principal do the same
thing over the console instead of a terminal. `install` additionally **stages
community code** fetched from ClawHub onto the operator's machine, so a
compromised pairing token also grants remote skill
install/enable/disable/uninstall; see CLAUDE.md §3.6 for the exposure-boundary
tradeoff. None of these routes work when `gateway.require_pairing = false`
opens the whole `/api/v1/*` surface, same as every other handler here.

### Skills are addressed by `slug`, not by `name`

Every `{slug}` path parameter below is the skill's **directory name**. A
skill's manifest `name:` is free text — `author_skill` and the console both
write the display name the user asked for, so a skill called `Kopi Pagi` lives
in `kopi-pagi/`. The routes run the path parameter through the same
`validate_slug` guard as ClawHub references, which rejects spaces, so passing
a display name containing one answers `400`.

Take the address from the `slug` field on `GET /api/v1/skills`; never
construct it from `name`. For every ClawHub and bundled skill the two are
identical, which is why this only started to matter once skills could be
authored locally.

The single-skill read routes also accept a manifest name as a fallback, so
clients written before `slug` existed keep working. The mutating routes do
not — `validate_slug` runs first there.

### GET /api/v1/skills

- **Auth**: bearer-gated.
- **Request**: none.
- **Response** `200`:
  ```json
  {
    "skills": [
      {
        "name": "...",
        "slug": "...",
        "version": "...",
        "description": "...",
        "tags": ["..."],
        "tools": ["tool_name_a", "tool_name_b"],
        "enabled": true,
        "active": true,
        "reasons": [],
        "origin": { "kind": "authored", "source": null },
        "clawhub": {
          "owner": "steipete",
          "slug": "weather",
          "version": "1.0.0",
          "reference": "@steipete/weather"
        }
      }
    ],
    "count": 1
  }
  ```
  `slug` is the skill's directory name and **the address every other skill
  route takes** (see above). It is omitted for entries with no directory of
  their own — open-skills files, which live flat in a shared checkout. Those
  cannot be acted on at all, so a client must not offer edit, enable, or
  uninstall on a row without one.

  `origin` records **who put the skill on disk**, as the gateway resolved it.
  `kind` is one of `authored`, `clawhub`, `bundled`, `git`, `local`. It is
  read from a `.origin.json` marker beside the skill's `SKILL.md`, falling
  back — only when no marker exists — to an inference from the directory's
  shape for skills that predate the marker. **It is omitted when the origin
  could not be established, and a client must read absence as "not editable",
  never as "probably fine".** Only `authored` unlocks the content routes
  below.

  `clawhub` says which publisher's copy is installed, read from the
  `.clawhub.json` marker beside the skill's `SKILL.md`. It is **omitted**, not
  null-filled, when there is no marker — which covers skills that did not come
  from ClawHub (bundled, git remote, local path) *and* ClawHub installs made
  before the marker existed. Absence therefore means *unattributed*, not "not
  from ClawHub"; a client must not read it as proof of either.

  `reference` is the value to send to `POST /api/v1/skills/install`, and stays
  a bare slug when the marker records no owner (installed back when the slug
  resolved without one). Clients comparing an installed skill against a
  ClawHub listing should match on `clawhub.reference`, or on `clawhub.slug`
  when the publisher is unknown — matching on `name` is wrong, since the
  manifest name can differ from the on-disk slug.

  `tools` here is just the tool **names** the skill exposes. `enabled`
  reflects only `[skills.entries.<name>] enabled` in `config.toml` (default
  `true`) — it's what the `PUT .../enabled` route below flips. `active` is
  `true` only when the skill is both enabled **and** its `requires` gates
  (binaries on `$PATH`, env vars, OS) are met; `reasons` lists why it isn't,
  with `"disabled in config.toml"` first when the config flag is off.
  Disabled/gated skills are still included in this list (previously they were
  silently dropped, which is why the console's toggle always looked "on").
- **Status codes**: `200`, `401`.

### GET /api/v1/skills/{slug}

- **Auth**: bearer-gated.
- **Path param**: `slug` — the directory name, matched case-insensitively.
  Falls back to matching the manifest `name` so pre-`slug` clients keep
  working.
- **Response** `200`: same shape as one entry in the list endpoint, plus a
  richer `tools`:
  ```json
  {
    "name": "...",
    "slug": "...",
    "version": "...",
    "description": "...",
    "tags": ["..."],
    "tools": [{ "name": "...", "description": "..." }],
    "enabled": true,
    "active": true,
    "reasons": [],
    "origin": { "kind": "authored", "source": null }
  }
  ```
  Unlike the list endpoint, `tools` here includes each tool's description.
  `enabled`/`active`/`reasons`/`slug`/`origin` have the same meaning as on the
  list endpoint.
- **Status codes**: `200`, `404` (no skill with that slug or name), `401`.

### POST /api/v1/skills/install

- **Auth**: bearer-gated.
- **Request**:
  ```json
  { "slug": "@steipete/weather" }
  ```
  `slug` accepts either a bare slug (`weather`) or the publisher-qualified
  form (`@steipete/weather`). Each segment is validated against ClawHub's slug
  charset (`[a-z0-9-_]`, no `\` or `..`) before anything is fetched.
- **Response** `200`:
  ```json
  { "slug": "@steipete/weather", "installed": true }
  ```
  `slug` echoes the reference exactly as sent, qualified or not; the skill
  itself is stored under its bare slug. Installing an already-installed skill
  is idempotent and still returns `installed: true` — it *is* installed,
  nothing was re-fetched. Asking for a slug that is installed from a
  *different* publisher is refused rather than reported as installed.
- **Response** `409` — the bare slug is published by more than one owner:
  ```json
  {
    "error": "ambiguous_skill_slug",
    "detail": "`weather` is published by 4 owners on ClawHub. Retry with one of the listed `reference` values.",
    "matches": [
      { "owner": "steipete", "reference": "@steipete/weather", "url": "https://clawhub.ai/steipete/skills/weather", "downloads": 165212, "official": true },
      { "owner": "lfengwa2", "reference": "@lfengwa2/weather", "url": "https://clawhub.ai/lfengwa2/skills/weather", "downloads": 57, "official": false }
    ]
  }
  ```
  Each `reference` can be sent straight back as the next request's `slug`.
  `downloads` and `official` are joined in from ClawHub's search index so the
  choice can be an informed one — among the four `weather` publishers, one is
  a verbatim fork of the top one with an identical name and summary. Both are
  best-effort: `0` / `false` means *unknown* (the lookup failed), not that the
  skill is unused. Candidates keep the order ClawHub returned them in; they
  are deliberately **not** sorted by popularity, since putting the largest
  number first would read as a recommendation.

  The server never picks a publisher for you: an install stages code the agent
  will later read and act on, so choosing by popularity or list order would
  hand a slug squatter a path onto the machine. `matches` is omitted from
  every other error response.
- **Status codes**: `200`, `400` (invalid reference), `401`, `409` (ambiguous
  slug), `500` (ClawHub fetch/hash/install failure).

### PUT /api/v1/skills/{slug}/enabled

- **Auth**: bearer-gated.
- **Path param**: `slug` — the directory name. `validate_slug` runs first, so
  a display name containing a space is rejected before resolution.
- **Request**:
  ```json
  { "enabled": false }
  ```
- **Response** `200`:
  ```json
  { "name": "weather", "enabled": false }
  ```
  Writes `[skills.entries.<name>] enabled` (an existing config key — see
  `docs/reference/config.md`) and persists it, the same as `rantaiclaw skills
  enable`/`disable`. Note the response and the config key both use the
  **manifest name**, not the slug: the route takes a slug in and resolves it,
  but the config contract is unchanged.
- **Status codes**: `200`, `400` (invalid slug), `401`, `404` (no skill with
  that slug), `500`.

### DELETE /api/v1/skills/{slug}

- **Auth**: bearer-gated.
- **Path param**: `slug` — the directory name.
- **Response** `200`:
  ```json
  { "name": "weather", "removed": true }
  ```
  Uninstalls the same way `rantaiclaw skills remove` does, including its
  path-traversal reject and 3-root containment gate (the removed directory
  must resolve under one of the known skill roots).
- **Status codes**: `200`, `400` (invalid slug), `401`, `404` (no skill with
  that slug), `500`.

### Authoring: read, write, and create a skill body

These three carry the console's skill editor. They are the only routes that
touch a `SKILL.md` body, and they exist because `GET /api/v1/skills/{slug}`
returns parsed metadata only — the body is never in it.

**All three refuse any skill whose `origin.kind` is not `authored`, with
`403`.** That is not a courtesy. A skill's whole file becomes part of the
agent's system prompt on the next load, so a route that rewrites one rewrites
the agent's standing instructions — the same reasoning that makes
`author_skill` and `skills_install` owner-only tools. Without the gate a
caller could replace vendor-reviewed content while the console still showed
the trusted badge.

`403` rather than `404` is deliberate: the caller can already list the skill
and read its metadata, so hiding it here would make the API disagree with
itself.

Request bodies are capped at 64 KiB by the shared body-limit layer, and JSON
escaping inflates newline-dense markdown — a `SKILL.md` near 58 KB can cross
the cap once encoded. Reads are unaffected (the cap is on requests). Clients
should check the encoded size and say so plainly rather than surfacing a bare
`413`.

#### GET /api/v1/skills/{slug}/content

- **Auth**: bearer-gated. Authored-only.
- **Response** `200`:
  ```json
  { "slug": "kopi-pagi", "name": "Kopi Pagi", "content": "---\nname: Kopi Pagi\n..." }
  ```
  `content` is the file verbatim, including frontmatter.
- **Status codes**: `200`, `400` (invalid slug), `401`, `403` (not authored),
  `404`, `500`.

#### PUT /api/v1/skills/{slug}/content

- **Auth**: bearer-gated. Authored-only.
- **Request**: `{ "content": "---\nname: Kopi Pagi\n..." }`
- **Response** `200`: `{ "slug": "...", "name": "...", "written": true }`

  The body must parse as frontmatter with a non-empty `name:`, and that name
  must equal the current one **exactly** — byte-for-byte, including case.
  Renaming is not supported here and is refused rather than half-applied: the
  name is the `[skills.entries.<name>]` config key, so changing even its case
  orphans the entry and silently resets whether the skill is enabled, while
  the directory keeps its old slug.

  The write is staged and renamed, never truncating in place — a half-written
  `SKILL.md` still parses as *something*, and that something would become the
  agent's instructions.
- **Status codes**: `200`, `400` (unparseable frontmatter, or a rename),
  `401`, `403`, `404`, `413` (body over 64 KiB), `500`.

#### POST /api/v1/skills

- **Auth**: bearer-gated.
- **Request**: `{ "name": "Kopi Pagi", "content": "---\nname: Kopi Pagi\n..." }`

  `name` is advisory. The `name:` **inside `content`** is what the loader
  reads and what the slug is derived from, so it wins if the two disagree —
  otherwise the directory and the manifest would disagree from the moment of
  creation.
- **Response** `201`: `{ "name": "Kopi Pagi", "slug": "kopi-pagi", "created": true }`

  Creates the directory under the active profile's skills root and writes
  `SKILL.md` plus a `.origin.json` marker with `kind: "authored"` — which is
  what makes the skill editable afterwards.

  Collisions are checked on **both** keys, across every skill root: the
  manifest name, and the derived slug. Two different display names can
  slugify to one directory, and the loader dedupes by name with the first
  root winning — so checking only one key leaves the other collision
  reachable, and a shadowed skill stops working with no error anywhere.
- **Status codes**: `201`, `400` (unparseable frontmatter, or a name with no
  characters usable in a directory name), `401`, `409` (name or slug taken),
  `413`, `500`.

---

## Memory

### GET /api/v1/memory

- **Auth**: bearer-gated.
- **Query params** — all optional, and all of them narrow rather than expand:
  - `limit` — default `50`, capped at `500`.
  - `offset` — rows to skip, newest first. Default `0`.
  - `category` — one category only. Unknown names are treated as custom
    categories, matching what `POST /api/v1/memory` accepts on write.
  - `q` — keyword search. When present and non-empty the read is served by
    the backend's ranked recall instead of a plain list, so entries come back
    ordered by relevance and carry a `score`. Composes with `category`.
- **Response** `200`:
  ```json
  {
    "entries": [
      {
        "key": "...",
        "category": "core",
        "content": "...",
        "timestamp": "...",
        "session_id": "may be null",
        "score": 0.87
      }
    ],
    "count": 1,
    "total": 121,
    "listed": 121,
    "offset": 0
  }
  ```
  `category` is one of `"core"`, `"daily"`, `"conversation"`, or a
  custom category string. **`timestamp` here is a string**, not the Unix
  epoch integer sessions/messages use elsewhere in this API — the memory
  backend and the sessions store encode time differently; this is a real,
  current inconsistency across resource groups worth knowing about if you're
  writing a client that parses both.

  `score` is relevance in `0.0..=1.0`, **relative to the best hit in the same
  result set** rather than an absolute measure. Only a `q` search ranks, so
  the field is `null` on a plain list.

  Counts are three different things and it is worth keeping them apart:
  `count` is how many entries this response carries, `listed` is how many the
  backend returned before `offset`/`limit` windowed them, and `total` is the
  size of the set you are paging. For an unfiltered read `total` is the whole
  store; when `category` or `q` narrows the read, `total` is the size of the
  narrowed set — otherwise a filtered page would advertise a total it could
  never reach. A `q` search ranks up to 500 hits before paging, so `total`
  stays put as you page through it.

  The handler fetches the entry list from the backend and windows it in the
  response — `limit` bounds the response size, not the underlying query.
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
