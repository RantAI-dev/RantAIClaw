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
| `usage` | `model`, `prompt`, `completion`, `total`, `cost_usd` | Token/cost summary when available. |
| `tool_call_start` | `id`, `name`, `args` | Agent started a tool call. |
| `tool_call_end` | `id`, `ok`, `output_preview` | Agent finished a tool call. |
| `error` | `message` | Non-recoverable turn error. |
| `done` | `text`, `cancelled` | Terminal event. The stream closes after this event. |

Completed non-cancelled streams are persisted to `sessions.db` with
`source = "api"`, matching the sync path. If the client disconnects before
`done`, the in-flight agent turn is cancelled and no API session is recorded.

Clients that omit `Accept: text/event-stream` and `?stream=1` keep the original
sync JSON response shape:

```json
{"text":"...","model":"...","provider":"...","duration_ms":1234}
```
