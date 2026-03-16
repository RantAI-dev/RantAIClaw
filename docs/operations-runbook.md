# RantaiClaw Operations Runbook

This runbook is for operators who maintain availability, security posture, and incident response.

Last verified: **February 18, 2026**.

## Scope

Use this document for day-2 operations:

- starting and supervising runtime
- health checks and diagnostics
- safe rollout and rollback
- incident triage and recovery

For first-time installation, start from [one-click-bootstrap.md](one-click-bootstrap.md).

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

## Logs and Diagnostics

### macOS / Windows (service wrapper logs)

- `~/.rantaiclaw/logs/daemon.stdout.log`
- `~/.rantaiclaw/logs/daemon.stderr.log`

### Linux (systemd user service)

```bash
journalctl --user -u rantaiclaw.service -f
```

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

4. If channels still fail, verify allowlists and credentials in `~/.rantaiclaw/config.toml`.

5. If gateway is involved, verify bind/auth settings (`[gateway]`) and local reachability.

## Safe Change Procedure

Before applying config changes:

1. backup `~/.rantaiclaw/config.toml`
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

- [one-click-bootstrap.md](one-click-bootstrap.md)
- [troubleshooting.md](troubleshooting.md)
- [config-reference.md](config-reference.md)
- [commands-reference.md](commands-reference.md)
