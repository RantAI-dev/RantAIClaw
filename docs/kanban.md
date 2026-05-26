# Kanban — Multi-Agent Profile Collaboration

RantaiClaw Kanban is a durable SQLite-backed task board, shared across all your RantaiClaw profiles, that lets multiple named agents collaborate on work without fragile in-process subagent swarms. Every task is a row in `~/.rantaiclaw/kanban.db`; every handoff is a row anyone can read and write.

This is a 1:1 port of the Hermes Agent kanban subsystem at v1 schema parity, so anyone familiar with `hermes kanban …` can apply the same playbooks here. See `docs/superpowers/specs/2026-05-16-kanban-port-design.md` for the design.

## Two surfaces: the model talks through tools, you talk through the CLI

The board has two front doors, both backed by the same `~/.rantaiclaw/kanban.db`:

- **Agents drive the board through a dedicated `kanban_*` toolset** — `kanban_show`, `kanban_list`, `kanban_complete`, `kanban_block`, `kanban_heartbeat`, `kanban_comment`, `kanban_create`, `kanban_link`, `kanban_unblock`. The dispatcher spawns each worker with these tools already in its schema; orchestrator profiles can opt in via `RANTAICLAW_KANBAN_ORCHESTRATOR=1`.
- **You (and scripts, and cron) drive the board through `rantaiclaw kanban …`** on the CLI, `/kanban …` as a slash command, or directly from a gateway channel.

Both surfaces route through the same `kanban::store` layer, so reads see a consistent view and writes can't drift.

## Core concepts

- **Board** — a standalone queue of tasks with its own SQLite DB. A single install can have many boards (one per project, repo, or domain). Single-project users stay on the `default` board and never see the word "board".
- **Task** — title, optional body, one assignee (a profile name), status (`triage | todo | ready | running | blocked | done | archived`), optional tenant namespace, optional idempotency key (dedup for retried automation).
- **Link** — `task_links` row recording a parent → child dependency. The dispatcher promotes `todo → ready` when all parents are `done`.
- **Comment** — the inter-agent protocol. Agents and humans append comments; when a worker is (re-)spawned it reads the full comment thread as part of its context.
- **Workspace** — the directory a worker operates in. Three kinds: `scratch` (default), `dir:<path>` (existing shared dir), `worktree` (git worktree). Workspace subprocess spawning is a designed-for extension point — see "Worker spawn" below.
- **Dispatcher** — a long-lived loop that reclaims stale claims, reclaims crashed workers, promotes ready tasks, atomically claims. Runs inside the gateway by default.
- **Tenant** — optional string namespace within a board.

## Paths

```
$RANTAICLAW_HOME/                          (default ~/.rantaiclaw)
├── kanban.db                              (board="default", back-compat shape)
└── kanban/
    ├── current                            (active board slug)
    └── boards/
        └── <slug>/
            ├── kanban.db
            ├── board.json
            ├── workspaces/
            └── logs/
```

Env overrides:
- `RANTAICLAW_KANBAN_HOME` — pin umbrella root (Docker / tests)
- `RANTAICLAW_KANBAN_DB` — pin DB file path directly
- `RANTAICLAW_KANBAN_BOARD` — pin active board slug (set by dispatcher on worker spawn)
- `RANTAICLAW_KANBAN_TASK` — pin worker's task id; flips on the `kanban_*` tool surface
- `RANTAICLAW_KANBAN_ORCHESTRATOR` — enable orchestrator-only tools on the active profile

## Quick start

```bash
# 1. Create the board
rantaiclaw kanban init

# 2. Create a task
rantaiclaw kanban create "research AI funding landscape" --assignee researcher

# 3. See the board
rantaiclaw kanban list
rantaiclaw kanban stats

# 4. Watch activity live
rantaiclaw kanban watch
```

### Boards (multi-project)

```bash
rantaiclaw kanban boards list
rantaiclaw kanban boards create atm10-server --name "ATM10 Server" --switch
rantaiclaw kanban --board atm10-server list
rantaiclaw kanban boards switch atm10-server
rantaiclaw kanban boards show              # who's active right now?
rantaiclaw kanban boards rename atm10-server "ATM10 (Prod)"
rantaiclaw kanban boards rm atm10-server   # archive (recoverable)
rantaiclaw kanban boards rm atm10-server --delete   # hard delete
```

Slugs are validated: lowercase alphanumerics + hyphens + underscores, 1-64 chars, must start with alphanumeric.

### Idempotent create (for automation / webhooks)

```bash
rantaiclaw kanban create "nightly ops review" \
    --assignee ops \
    --idempotency-key "nightly-ops-$(date -u +%Y-%m-%d)" \
    --json
```

### Bulk verbs

```bash
rantaiclaw kanban complete t_abc t_def t_hij --result "batch wrap"
rantaiclaw kanban archive  t_abc t_def t_hij
rantaiclaw kanban unblock  t_abc t_def
rantaiclaw kanban block    t_abc "need input" --ids t_def t_hij
```

## CLI surface

```
rantaiclaw kanban init
rantaiclaw kanban create "<title>" [--body] [--assignee] [--parent]... [--tenant]
                                   [--workspace scratch|worktree|dir:<path>]
                                   [--priority N] [--triage] [--idempotency-key K]
                                   [--max-runtime 30m|2h|<seconds>] [--skill <name>]...
                                   [--max-retries N] [--json]
rantaiclaw kanban list [--mine] [--assignee P] [--status S] [--tenant T] [--archived] [--json]
rantaiclaw kanban show <id> [--json]
rantaiclaw kanban assign <id> <profile|none>
rantaiclaw kanban link <parent_id> <child_id>
rantaiclaw kanban unlink <parent_id> <child_id>
rantaiclaw kanban claim <id> [--ttl SECONDS]
rantaiclaw kanban comment <id> "<text>" [--author NAME]
rantaiclaw kanban complete <id>... [--result] [--summary] [--metadata JSON]
rantaiclaw kanban block <id> "<reason>" [--ids <id>...]
rantaiclaw kanban unblock <id>...
rantaiclaw kanban archive <id>...
rantaiclaw kanban tail <id>
rantaiclaw kanban watch [--assignee P] [--tenant T] [--kinds k1,k2] [--interval SECS]
rantaiclaw kanban heartbeat <id> [--note]
rantaiclaw kanban runs <id> [--json]
rantaiclaw kanban assignees [--json]
rantaiclaw kanban dispatch [--dry-run] [--max N] [--failure-limit N] [--json]
rantaiclaw kanban stats [--json]
rantaiclaw kanban notify-subscribe <id> --platform P --chat-id C [--thread-id T] [--user-id U]
rantaiclaw kanban notify-list [<id>] [--json]
rantaiclaw kanban notify-unsubscribe <id> --platform P --chat-id C [--thread-id T]
rantaiclaw kanban context <id>
rantaiclaw kanban specify [<id> | --all] [--tenant T] [--author NAME] [--json]
rantaiclaw kanban boards {list | create | switch | show | rename | rm}
rantaiclaw kanban --board <slug> <subcommand>          # one-shot board scope
```

## `/kanban` slash command

Every `rantaiclaw kanban <action>` verb is also reachable as `/kanban <action>` — from inside the interactive TUI and (when wired) from any gateway channel. The slash command reuses the same clap subcommand tree the CLI does, so arguments, flags, and output format are identical.

```
/kanban list
/kanban show t_abcd
/kanban create "write launch post" --assignee writer --parent t_research
/kanban comment t_abcd "looks good, ship it"
/kanban unblock t_abcd
/kanban dispatch --max 3
/kanban specify t_abcd
```

Quote multi-word arguments — the rest of the line is parsed with shell-style splitting.

## Agent tools (`kanban_*`)

| Tool | Purpose | When schema is active |
|---|---|---|
| `kanban_show` | Read the current task | `RANTAICLAW_KANBAN_TASK` set OR `RANTAICLAW_KANBAN_ORCHESTRATOR` set |
| `kanban_list` | List task summaries with filters | orchestrator |
| `kanban_complete` | Finish with `summary` + `metadata` structured handoff | worker or orchestrator |
| `kanban_block` | Escalate for human input | worker or orchestrator |
| `kanban_heartbeat` | Signal liveness during long operations | worker |
| `kanban_comment` | Append a durable note to the task thread | worker or orchestrator |
| `kanban_create` | Fan out into child tasks | orchestrator |
| `kanban_link` | Add a `parent_id → child_id` dependency edge after the fact | orchestrator |
| `kanban_unblock` | Move a blocked task back to `ready` | orchestrator |

A typical worker turn:

```
# Model's tool calls, in order:
kanban_show()                                     # no args — uses RANTAICLAW_KANBAN_TASK
# (model does the work via terminal/file tools)
kanban_heartbeat(note="halfway through")
kanban_complete(
    summary="migrated limiter.rs to token-bucket; added 14 tests, all pass",
    metadata={"changed_files": ["limiter.rs", "tests/test_limiter.rs"], "tests_run": 14},
)
```

An orchestrator fans out instead:

```
kanban_show()
kanban_create(title="research ICP funding 2024-2026", assignee="researcher-a", body="...")
kanban_create(title="research ICP funding — EU angle", assignee="researcher-b", body="...")
kanban_create(
    title="synthesize findings into launch brief",
    assignee="writer",
    parents=["t_r1", "t_r2"],                     # promotes to ready when both complete
    body="one-pager, 300 words, neutral tone",
)
kanban_complete(summary="decomposed into 2 research tasks + 1 writer; linked dependencies")
```

Tools short-circuit on `execute()` when the env vars aren't set, returning a structured "unavailable" message. This keeps `rantaiclaw chat` sessions free of irrelevant kanban tools without compile-time feature gates.

## Runs — one row per attempt

A task is a logical unit; a **run** is one attempt to execute it. When the dispatcher claims a `ready` task it creates a row in `task_runs` and points `tasks.current_run_id` at it. When the attempt ends, the run row closes with an `outcome`. A task that's been attempted three times has three `task_runs` rows.

```
rantaiclaw kanban runs t_abcd
#1   blocked   worker      "need decision on rate-limit key"
#2   completed worker      "implemented token bucket, keys on user_id with IP fallback"
```

Runs are where structured handoff lives. `kanban_complete(summary, metadata)` puts `summary` and `metadata` on the closing run, and downstream children see them in their `build_worker_context`. When `complete_task` is called on a never-claimed task with `summary`/`metadata`, a zero-duration run is synthesized so attempt history stays complete.

## Event reference

Every transition appends a row to `task_events`. Each row carries an optional `run_id` so UIs can group events by attempt. Kinds:

**Lifecycle**: `created`, `promoted`, `claimed`, `completed`, `blocked`, `unblocked`, `archived`

**Edits**: `assigned`, `edited`, `reprioritized`, `status`

**Worker telemetry**: `spawned`, `heartbeat`, `reclaimed`, `claim_extended`, `claim_rejected`, `crashed`, `timed_out`, `spawn_failed`, `gave_up`, `completion_blocked_hallucination`, `suspected_hallucinated_references`

`rantaiclaw kanban tail <id>` shows these for a single task. `rantaiclaw kanban watch` streams them board-wide.

## Gateway notifications

`kanban_notify_subs` pairs a chat (platform + chat + thread) with a task. The gateway notifier loop tails `task_events`, joins on subscriptions, and pushes one message per terminal event (`completed`, `blocked`, `gave_up`, `crashed`, `timed_out`) back to the originating chat.

```bash
rantaiclaw kanban notify-subscribe t_abcd \
    --platform telegram --chat-id 12345678 --thread-id 7
rantaiclaw kanban notify-list
rantaiclaw kanban notify-unsubscribe t_abcd \
    --platform telegram --chat-id 12345678 --thread-id 7
```

Subscriptions auto-remove themselves once the task hits `done` or `archived`.

## Triage & specifier

A task created with `--triage` parks in the `triage` column and the dispatcher leaves it alone. Run `rantaiclaw kanban specify <id>` to expand the one-liner into a structured spec (goal / approach / acceptance criteria) and promote it to `todo`. `--all` sweeps every triage task at once.

The current specifier is a deterministic template — a follow-up wires an auxiliary LLM here without touching the CLI surface.

## Worker spawn (extension point)

Hermes shells out `hermes -p <profile>` per claim. RantaiClaw's worker model is still being shaped, so the dispatcher ships without a default spawner. `Dispatcher::set_spawner(|task_id| { ... })` accepts a callable that receives the just-claimed task — wire your own subprocess (or in-process agent loop) into this hook to make the dispatcher actually run work.

When no spawner is set, the dispatcher claims and leaves the task in `running`; the operator can complete/block via CLI to keep the lifecycle moving.

## Out of scope (this PR)

- React/web kanban dashboard. RantaiClaw has no web dashboard yet; the REST shape is reserved for when one lands.
- Auxiliary-LLM-driven `specify`. The CLI / tool surface lands with a deterministic template.
- Workspace subprocess spawning. Schema persists `workspace_kind` / `workspace_path`; the dispatcher exposes the env vars; the actual spawn is a 50-line `set_spawner` away once the team picks the worker model.

## Spec

The complete design — architecture, schema rationale, comparison with Hermes — lives in `docs/superpowers/specs/2026-05-16-kanban-port-design.md`. Read that before filing any behaviour-change PR.
