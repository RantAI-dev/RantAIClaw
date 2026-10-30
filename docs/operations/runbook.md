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

Profiles and the installed service: a service installed without an explicit profile
(`--profile <name>` / `RANTAICLAW_PROFILE`) follows the active-profile marker, so
`rantaiclaw profile use <name>` restarts the unit onto the new profile. On systemd and
launchd, a service installed with an explicit profile is pinned to it: `profile use` leaves it
running and says so; reinstall from the other profile to move it. On OpenRC, `service install`
refuses an explicit profile, because the unit reads its config from `/etc/rantaiclaw` and a
profile cannot apply to it; install it again without a profile, and the unit follows the
active-profile marker.

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

5. Check today's token usage against the daily ceiling (UTC day). Turns are refused once the
   ceiling is spent, so check this when a run stops with a ceiling error:

```bash
rantaiclaw usage
```

## Health and State Signals

| Signal | Command / File | Expected |
|---|---|---|
| Config validity | `rantaiclaw doctor` | no critical errors |
| Channel connectivity | `rantaiclaw channel doctor` | configured channels healthy |
| Runtime summary | `rantaiclaw status` | expected provider/model/channels |
| Daemon heartbeat/state | `~/.rantaiclaw/daemon_state.json` | file updates periodically; it is removed when the daemon stops, and a file older than ~15 seconds is treated as "daemon not running" (relevant after a crash) |
| Session persistence | `rantaiclaw session list` or `GET /api/v1/sessions` | recent CLI/TUI/API turns are listed |

### The daemon rows of `rantaiclaw doctor`

`rantaiclaw doctor` reports two rows about the daemon: `daemon.scheduler` and
`daemon.registration`. Both read local state — the daemon's own state file and the host
service manager — so both run under `--brief` and in the console's brief mode.

| Row | Verdict | Meaning | What to do |
|---|---|---|---|
| `daemon.scheduler` | `healthy` | The daemon is running and its scheduler component reports `ok` | nothing |
| `daemon.scheduler` | `unhealthy — see daemon logs` | The daemon is running and its scheduler component reports `error` | read the daemon log (`journalctl --user -u rantaiclaw.service -f` on Linux) |
| `daemon.scheduler` | `daemon not running …` | No state file, or one the daemon has stopped refreshing (older than ~15 s) | `rantaiclaw service start` (or `rantaiclaw daemon`) |
| `daemon.scheduler` | `not tracked yet` | The daemon is running but has not published a scheduler component | normally transient during startup; re-run `doctor` after a few seconds |
| `daemon.scheduler` | a status string this build does not know (for example `starting`) | The daemon reported a scheduler status the build does not recognise | read the daemon log; the row is a warning, not a failure |
| `daemon.registration` | `daemon registered via systemd (user) as unit rantaiclaw.service (runs <path>)` | This installation's unit is installed and runs this binary | nothing |
| `daemon.registration` | `… but runs <path> — not this binary (<path>)` | The unit that answered belongs to a different installation | `rantaiclaw service install` to point the unit at this binary |
| `daemon.registration` | `daemon not registered with <manager> (optional) — looked for <units>` | No unit of that name is installed | `rantaiclaw service install` (optional) |
| `daemon.registration` | `daemon registered with <manager> as unit <unit> but not running` | The unit is installed but stopped | `rantaiclaw service start` |
| `daemon.registration` | `unit <unit> is registered with <manager> and is active, but the binary it runs could not be verified — not confirmed as this installation` | The unit is installed and running, but the manager named no executable the check could compare, so the unit may belong to another installation | check the unit's `ExecStart` to see which binary it runs |
| `daemon.registration` | `unit <unit> is registered with <manager> but is not running, and the binary it runs could not be verified — not confirmed as this installation` | The unit is installed but stopped, and the binary it runs could not be verified | check the unit's `ExecStart` to see which binary it runs |
| `daemon.registration` | `could not query <manager> for unit <unit> — the user service manager did not answer; registration status unknown` | The service manager could not be reached (no user session bus, for one), so the unit's presence is unknown — neither registered nor absent | run `doctor` in a session the user manager serves; `systemctl --user status` has to answer |

The `daemon.registration` row also prints two lines that are informational rather than
registration verdicts. The first is `init system not detected (skipped)`, printed when
the host has no init system the check can ask. The second is
`development build does not talk to the service manager`, printed when the binary is a
debug or test build — the guard that keeps a developer's build away from the operator's
service manager. Both are `info`.

Within these two rows, only the `unhealthy` scheduler verdict is a failure. Every other
scheduler verdict and every registration verdict is reported as `ok`, `warn` or `info` —
including the two registration verdicts that report what could not be established: a unit
whose binary could not be verified (`not confirmed as this installation`), and a service
manager that did not answer (`registration status unknown`).

That is about these rows alone. `rantaiclaw doctor` exits non-zero when any check it
runs reports a `Fail` verdict — a warning never changes the exit code — and
`config.schema` is one of those checks: a config that does not validate fails the doctor
on its own.

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

## Daemon Stops With Exit Code 78

A daemon that exits with code 78 (`EX_CONFIG`) stopped for something that can never work; the
stop message names what to fix. The fatal set is:

- an unknown tunnel `provider` name, or a missing `[tunnel.<provider>]` section
- a refused public bind (`[gateway] allow_public_bind` is false)
- a bind host that is neither a socket address nor a valid hostname
- a privileged port (below 1024) that cannot be bound
- an unknown `default_provider`
- a custom provider URL that is missing, unparseable, or not http(s)
- an unknown `[memory] backend`
- an unknown `[runtime] kind`
- a profile selection the door refuses (`--profile <name>` or `RANTAICLAW_PROFILE` naming a
  profile that cannot be selected)
- the active-profile marker names a profile that does not exist
- a `config.toml` that does not parse as TOML or does not fit the schema
- a `config.toml` value the schema refuses
- a `config.toml` secret the `.secret_key` on disk cannot decrypt
- a `schema_version` that is negative, malformed or newer than the binary supports

A missing API key, a missing auth profile and a DNS lookup failure are NOT in that set: they are
retried, because `rantaiclaw auth login`, an environment variable, a key stored in the config later
or a resolver that comes back can supply them without the config file changing. A secret that is
present but that the key on disk cannot decrypt is a different case — the files on disk have to
change before the daemon can start.

The profile-door refusal is NOT retried either: it names the profile it could not select, and the
daemon exits before it starts, so the operator has to fix the selection. The config-load refusals
are not retried either: the same files fail the same way on every start, so a restart cannot fix
them.

systemd units rendered by current versions carry `RestartPreventExitStatus=78`, so systemd does not
restart the daemon — fix the config and start the service again. On launchd (macOS) the job is
restarted anyway: launchd cannot exclude a single exit code, so it applies its own throttling and the
reason is in `~/.rantaiclaw/logs/daemon.stderr.log`.

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
   - The backup keeps failing, so no complete backup directory exists. A failed backup normally leaves the config on disk unwritten, but a config saved earlier in the same failed session may already differ, so check what the file names. While it names `markdown`, every start makes the backup again and writes `PENDING` again, and deleting `PENDING` alone does not stop that. Set `[memory] backend = "sqlite"` in the config, remove a `[storage.provider.config] provider = "markdown"` line if either is there, and then delete `<workspace>/memory/migrations/PENDING`. Nothing retries the backup after that, and the notes stay in the live markdown files.
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
