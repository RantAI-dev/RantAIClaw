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
