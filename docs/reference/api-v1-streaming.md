# API v1 Streaming

`POST /api/v1/agent/chat` supports Server-Sent Events for clients that want
partial assistant output.

Enable streaming with either:

- `Accept: text/event-stream`
- `?stream=1`

Example:

```bash
curl -N -X POST http://127.0.0.1:9091/api/v1/agent/chat \
  -H "Authorization: Bearer $TOKEN" \
  -H "Accept: text/event-stream" \
  -H "Content-Type: application/json" \
  -d '{"message":"Count to 5 slowly, one number per line."}'
```

Each event is emitted as a JSON payload in an SSE `data:` line:

```text
data: {"type":"chunk","text":"1\n"}
data: {"type":"usage","model":"...","prompt":10,"completion":20,"total":30,"cost_usd":0.0}
data: {"type":"done","text":"1\n2\n3\n4\n5\n","cancelled":false}
```

Event types:

| Type | Fields | Meaning |
|---|---|---|
| `chunk` | `text` | Assistant text delta. Multiple chunks may arrive per turn. |
| `usage` | `model`, `prompt`, `completion`, `total`, `cost_usd` | Token/cost summary. Emitted only when token counts are known — absence means the provider did not report usage. |
| `memory_recalled` | `keys` | Keys of stored memories injected into this turn's prompt. Emitted before the first chunk. |
| `tool_call_start` | `id`, `name`, `args` | Agent started a tool call. |
| `tool_call_end` | `id`, `ok`, `output_preview` | Agent finished a tool call. |
| `approval_request` | `id`, `tool`, `args` | Agent paused awaiting an in-browser approval. Resolve via `POST /api/v1/approvals/{id}`; the stream resumes after. |
| `approval_resolved` | `id`, `approved`, `timed_out` | The approval identified by `id` was answered (`approved` true/false) or expired (`timed_out` true). Close the modal. Scoped to the turn that raised the request. |
| `error` | `message` | Non-recoverable turn error. |
| `reload_complete` | — | Informational: a config reload completed (benign for a per-request gateway agent). |
| `compaction_start` | `original_count`, `keep_last` | Context compaction began. |
| `compaction_complete` | `summary`, `original_count`, `keep_last`, `kept_count` | Context compaction finished. |
| `done` | `text`, `cancelled`, `session_id` | Terminal event; `session_id` is the session this turn was persisted to (pass it back to continue). The stream closes after this event. |

Completed non-cancelled streams are persisted to `sessions.db` with
`source = "api"`, matching the sync path. If the client disconnects before
`done`, the in-flight agent turn is cancelled and no API session is recorded.

Clients that omit `Accept: text/event-stream` and `?stream=1` keep the sync
JSON response shape:

```json
{"text":"...","model":"...","provider":"...","duration_ms":1234,"session_id":"..."}
```
