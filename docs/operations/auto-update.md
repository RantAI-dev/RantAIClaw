# Auto-update strategies

`rantaiclaw update` is a one-shot binary self-replace; running it on a
schedule turns it into an auto-updater. This page documents the two
recommended patterns — the built-in cron scheduler and a systemd timer
— plus the rollback story when an update goes wrong.

## What every update does (whether manual or scheduled)

Every successful `rantaiclaw update`, whether you ran it by hand or
from cron, performs these steps in order:

1. **Resolve target tag** — latest stable, latest prerelease, or pinned
   `--to <tag>`.
2. **Compare versions** — refuse to downgrade unless `--allow-downgrade`.
3. **`--check` short-circuit** — if `--check` is set, print delta +
   release notes URL, exit 1 if behind / 0 if up-to-date, no files
   touched.
4. **Confirmation prompt** — skipped with `-y` / `--yes`. Cron jobs
   should always pass `--yes`.
5. **Pre-update state snapshot** — copies `config.toml`,
   `active_profile`, `active_workspace.toml`, the active profile's
   `config.toml` + `persona/`, into
   `~/.rantaiclaw/.update-snapshots/<UTC-timestamp>/`. Always runs;
   never aborts the update on failure (logged as a warning instead).
6. **Optional `--backup`** — full-profile tarball of the rantaiclaw
   home directory. Slower; opt-in for production / shared installs.
7. **Download archive + SHA256SUMS** from the release.
8. **Verify SHA256** of the archive against the line in `SHA256SUMS`.
9. **Extract the binary** using the system `tar` (no extra crates).
10. **Atomic swap** on Unix via `rename()`: previous binary moves to
    `rantaiclaw.old`, new binary takes its place. The `.old` is
    **kept** so `rantaiclaw rollback` can restore it.
11. **Restart managed daemon service** — if `rantaiclaw daemon` is
    running under systemd (Linux user) or launchd (macOS), the
    service manager is told to restart so the running process picks
    up the new binary instead of staying on the old in-memory code.

## Pattern 1 — built-in cron scheduler (recommended)

If you already use `rantaiclaw cron` for other tasks, adding update
checks costs nothing extra:

```bash
# Nightly check — exit 1 if behind, useful for shell-script gating
rantaiclaw cron add '0 4 * * *' 'rantaiclaw update --check'

# Weekly auto-pull, Sunday 4 AM
rantaiclaw cron add '0 4 * * 0' 'rantaiclaw update --yes'

# Production / shared install: prefer --backup so any post-swap
# config issue is recoverable without losing sessions/skills.
rantaiclaw cron add '0 4 * * 0' 'rantaiclaw update --yes --backup'
```

The cron task runs in the daemon's process tree and inherits its
environment. The newly-installed binary will be in effect the next
time the daemon restarts — and after a successful update, the daemon
restart is automatic (step 11 above).

## Pattern 2 — systemd timer (Linux, no daemon)

If you don't run `rantaiclaw daemon` and just want the binary kept
fresh:

```ini
# ~/.config/systemd/user/rantaiclaw-update.service
[Unit]
Description=Update rantaiclaw binary

[Service]
Type=oneshot
ExecStart=/usr/local/bin/rantaiclaw update --yes
```

```ini
# ~/.config/systemd/user/rantaiclaw-update.timer
[Unit]
Description=Weekly rantaiclaw update

[Timer]
OnCalendar=Sun *-*-* 04:00:00
RandomizedDelaySec=30m
Persistent=true

[Install]
WantedBy=timers.target
```

Enable:

```bash
systemctl --user daemon-reload
systemctl --user enable --now rantaiclaw-update.timer
systemctl --user list-timers rantaiclaw-update.timer
```

`Persistent=true` ensures missed runs (laptop suspended, machine off)
fire on next boot.

## Pattern 3 — launchd (macOS)

```xml
<!-- ~/Library/LaunchAgents/com.rantaiclaw.update.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>          <string>com.rantaiclaw.update</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/rantaiclaw</string>
        <string>update</string>
        <string>--yes</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Weekday</key>    <integer>0</integer>
        <key>Hour</key>       <integer>4</integer>
        <key>Minute</key>     <integer>0</integer>
    </dict>
    <key>RunAtLoad</key>      <false/>
    <key>StandardErrorPath</key>
    <string>/tmp/rantaiclaw-update.log</string>
</dict>
</plist>
```

```bash
launchctl load ~/Library/LaunchAgents/com.rantaiclaw.update.plist
```

## Rollback

When an update goes wrong:

```bash
rantaiclaw rollback                         # restore latest snapshot, prompt first
rantaiclaw rollback -y                      # skip prompt
rantaiclaw rollback --list                  # show available snapshots, no restore
rantaiclaw rollback --snapshot ~/.rantaiclaw/.update-snapshots/2026-05-09T03-21-00Z
```

`rollback` restores:

- The `.old` binary (renamed back to live, current binary moves to
  `.old` so you can re-roll forward if needed).
- Every state file the snapshot manifest lists (config.toml,
  active_profile, persona/, active profile's config.toml).
- The daemon service is restarted again so the rolled-back binary is
  what's running.

What `rollback` doesn't restore by default:

- **sessions.db** — large, regeneratable, snapshots intentionally skip
  it. If you need it, take `update --backup` first; it lands in
  `~/.rantaiclaw/.update-snapshots/rantaiclaw-backup-<version>-<timestamp>.tar.gz`
  and you can `tar xzf` it manually.
- **Skill installs** — Skills are kept in `~/.rantaiclaw/profiles/<n>/skills/`
  and aren't snapshotted. Re-run `rantaiclaw skills update --all` if a
  bundled skill changed shape across versions.

## When auto-update is the wrong choice

- **Pinned production**: prefer `--to v0.6.X-alpha` and review release
  notes before bumping. Use `update --check` in cron to monitor for
  newer versions without auto-pulling.
- **Air-gapped / offline mirrors**: set `RANTAICLAW_RELEASE_BASE_URL`
  to your internal mirror; cron-driven update will pull from there.
- **Multiple machines on the same network**: stagger `RandomizedDelaySec`
  (systemd) or run with random sleeps so they don't all hit GitHub
  at the same minute.

## Comparison with hermes-agent

Hermes does git-pull-driven updates (it's a Python source install). We
do binary-swap updates. Both ecosystems support:

- A pre-update state snapshot (always-on, lightweight) — ✅
- An opt-in full-profile backup tarball — ✅
- Service auto-restart after the swap — ✅
- A `--check` mode that exits 1 if behind, no side-effects — ✅
- A rollback command — ✅

What we additionally have over Hermes:

- **Cosign signature verification** of the released binary
  (Hermes relies on git auth)
- **Mirror override** via `RANTAICLAW_RELEASE_BASE_URL` for offline /
  internal-mirror deployments

What Hermes has over us:

- **Config-schema migration prompts** — Hermes prompts for newly-added
  config keys interactively. We don't, because TOML with
  `serde(default)` makes new fields auto-default; missing fields don't
  break loading. If your update introduces breaking config changes,
  rantaiclaw will *log* a warning at next launch via the schema's
  validation; consult the release notes (printed by `update --check`)
  for migration instructions.

## Related

- [`docs/commands-reference.md`](../commands-reference.md) — `update`
  / `rollback` flags
- [`docs/operations-runbook.md`](../operations-runbook.md) — production
  operations checklist
- `~/.rantaiclaw/.update-snapshots/` — snapshot directory layout
