# RantaiClaw Operations Runbook

This runbook is for operators who maintain availability, security posture, and incident response.

Last verified: **May 9, 2026**.

## Scope

Use this document for day-2 operations:

- starting and supervising runtime
- health checks and diagnostics
- safe rollout and rollback
- incident triage and recovery

For first-time installation, start from [one-click-bootstrap.md](../start/one-click-bootstrap.md).

## Runtime Modes

| Mode | Command | When to use |
|---|---|---|
| Foreground runtime | `rantaiclaw daemon` | local debugging, short-lived sessions |
| Foreground gateway only | `rantaiclaw gateway` | webhook endpoint testing |
| User service | `rantaiclaw service install && rantaiclaw service start` | persistent operator-managed runtime |

## Baseline Operator Checklist

1. Validate configuration:

```bash
rantaiclaw status
```

2. Verify diagnostics:

```bash
rantaiclaw doctor
rantaiclaw channel doctor
```

3. Start runtime:

```bash
rantaiclaw daemon
```

4. For persistent user session service:

```bash
rantaiclaw service install
rantaiclaw service start
rantaiclaw service status
```

## Health and State Signals

| Signal | Command / File | Expected |
|---|---|---|
| Config validity | `rantaiclaw doctor` | no critical errors |
| Channel connectivity | `rantaiclaw channel doctor` | configured channels healthy |
| Runtime summary | `rantaiclaw status` | expected provider/model/channels |
| Daemon heartbeat/state | `~/.rantaiclaw/daemon_state.json` | file updates periodically |
| Session persistence | `rantaiclaw session list` or `GET /api/v1/sessions` | recent CLI/TUI/API turns are listed |

## Session Persistence Checks

`agent -m`, the TUI, and `POST /api/v1/agent/chat` all write completed turns to
the shared `sessions.db`. API-created sessions use `source = "api"`.

Fast check:

```bash
curl -s http://127.0.0.1:9393/api/v1/sessions | jq .
```

If gateway pairing is required, include `Authorization: Bearer <token>`.

## Logs and Diagnostics

Channel logs identify a message by channel, sender, message id and length in characters, never by
its text, at any log level: `channel message received` and `channel reply` carry `message_id` and
`chars`, and the gateway logs the same fields when it receives a webhook message. Journals written by
0.31.0-alpha and earlier may still hold the start of each message and reply, including a pairing
code a user typed; upgrading does not rewrite them. A daemon started with Telegram's
`allowed_users` empty no longer writes its one-time pairing code to the journal. Mint a code with
`rantaiclaw channel pair --channel telegram` and DM the bot `/claim <code>`.

WhatsApp Web (the wa-rs library, plus the `wa_rs_libsignal` and `Client/*` targets) logs at `warn`
by default because its per-message `INFO` lines carry full LIDs and group JIDs — the journal holds
the diagnosis-relevant `warn` lines (failed device resolution, missing sender key, failed session
establishment) but not the per-message identifiers. To diagnose a WhatsApp Web delivery problem,
restart the daemon with `RUST_LOG=info,wa_rs=debug,Client=debug` for a diagnosis session; the
journal will then hold full LIDs and group JIDs, so do not leave it set. `EnvFilter` matches
targets by plain string prefix, so `wa_rs=debug` covers `wa_rs_libsignal` too and `Client=debug`
covers every `Client/*` target.

### macOS / Windows (service wrapper logs)

- `~/.rantaiclaw/logs/daemon.stdout.log`
- `~/.rantaiclaw/logs/daemon.stderr.log`

### Linux (systemd user service)

```bash
journalctl --user -u rantaiclaw.service -f
```

## Stopping and Restarting

`rantaiclaw service stop` and `rantaiclaw service restart` send SIGTERM, and the daemon drains
before it exits:

- The gateway and channels share one 16-second window to finish in-flight work.
- A reply still being written 12 seconds in is stopped, and its conversation gets one message in its
  own thread saying the bot is restarting and the message should be sent again. A message that was
  queued, or waiting for a free worker, gets the same message. The journal records each one as
  `sent a restart notice` with its `channel` and `message_id`.
- Nothing is replayed after the restart, so a turn that already ran tools does not run again.
- Auto-managed services stop after channels, inside the unit's `TimeoutStopSec=30`.

## One-time markdown memory import

A config that used the retired `markdown` memory backend imports `MEMORY.md` and `memory/*.md` into
`brain.db` once, on the first start after the upgrade. Before it imports, the runtime copies the
notes into a backup directory under the workspace:

```text
<workspace>/memory/migrations/markdown-<timestamp>-<pid>/
```

The directory holds `MEMORY.md`, `memory/*.md`, and a copy of `brain.db` at `memory/brain.db`. Markers
in `<workspace>/memory/migrations/` and in the backup directory show where the import stands:

| Marker | Location | Meaning |
|---|---|---|
| `BACKUP_COMPLETE` | backup directory | The backup is whole. It holds the cutoff time, and the import keeps any shared `brain.db` row newer than that time. The cutoff is the backup start time, or the earlier time held by `PENDING` when that marker exists. |
| `IMPORTED` | backup directory | The import committed. The runtime does not import this backup again. |
| `PENDING` | `<workspace>/memory/migrations/` | A backup failed and is still owed. Every start retries the backup and the import from the live files until one succeeds, then removes the marker. |

Two warnings mean the import is not finished. Both retry on their own at the next start:

- `failed to back up markdown memory before migrating the config to sqlite; the schema upgrade and the import both retry on the next start.`
- `markdown memory import failed; the config still loads as sqlite, but the original markdown notes were not migrated. The backup at <path> is intact and the import retries automatically on the next start.`

While `PENDING` exists, each start also logs `a markdown memory backup failed earlier and is still owed; retrying it from the live files`.

To recover by hand:

1. Read the error in the warning and fix its cause, which the `error` field names. Restart the runtime.
2. If a backup directory has no `BACKUP_COMPLETE`, the backup stopped partway and the runtime ignores it. The live `MEMORY.md` and `memory/*.md` are untouched until an import commits, so copy notes from them.
3. To stop the retries, act on the case you are in:
   - The backup keeps failing, so no complete backup directory exists. Delete `<workspace>/memory/migrations/PENDING`. Nothing retries the backup after that, and the notes stay in the live markdown files.
   - The backup is complete but its import keeps failing. Deleting `PENDING` does not stop it, because every start with the `sqlite` backend imports each backup directory that has `BACKUP_COMPLETE` and no `IMPORTED`. Create an empty `IMPORTED` file in that backup directory (`touch <backup directory>/IMPORTED`) and delete `PENDING` if it exists. The runtime then treats the backup as imported and skips it. The notes stay in the live markdown files and in the backup directory.

## Incident Triage Flow (Fast Path)

1. Snapshot system state:

```bash
rantaiclaw status
rantaiclaw doctor
rantaiclaw channel doctor
```

2. Check service state:

```bash
rantaiclaw service status
```

3. If service is unhealthy, restart cleanly:

```bash
rantaiclaw service stop
rantaiclaw service start
```

4. If channels still fail, verify allowlists and credentials in the active profile's config (`~/.rantaiclaw/profiles/<name>/config.toml`; run `rantaiclaw config` to print the resolved path).

5. If gateway is involved, verify bind/auth settings (`[gateway]`) and local reachability.

## Safe Change Procedure

Before applying config changes:

1. backup the active profile's config (`~/.rantaiclaw/profiles/<name>/config.toml`)
2. apply one logical change at a time
3. run `rantaiclaw doctor`
4. restart daemon/service
5. verify with `status` + `channel doctor`

## Rollback Procedure

If a rollout regresses behavior:

1. restore previous `config.toml`
2. restart runtime (`daemon` or `service`)
3. confirm recovery via `doctor` and channel health checks
4. document incident root cause and mitigation

## Related Docs

- [one-click-bootstrap.md](../start/one-click-bootstrap.md)
- [troubleshooting.md](../start/troubleshooting.md)
- [config-reference.md](../reference/config.md)
- [commands-reference.md](../reference/commands.md)
