# Kanban Port — 1:1 Hermes → RantaiClaw

**Status:** approved (user goal: "do your best dont defer anything to next phase")
**Branch:** `feat/kanban-port`
**Reference:** `/home/shiro/rantai/refs/hermes-agent/hermes_cli/kanban*.py`, `tools/kanban_tools.py`, `website/docs/user-guide/features/kanban.md`

## Goal

Port Hermes Kanban — a durable SQLite-backed multi-agent task board — into RantaiClaw at feature parity for everything that has a non-web surface. Map every Hermes verb (CLI, slash, tool) to a Rust equivalent. Multi-board, multi-tenant, runs/attempts, notify subscriptions, dispatcher loop, agent tools, TUI view.

## Out of scope (this PR)

- Web/React dashboard. RantaiClaw has no web dashboard subsystem yet — porting Hermes's React kanban plugin requires building one from scratch (separate PR scope). Designed-for-it (REST shape lives behind a `dashboard` feature gate that can be lit up later).
- Auxiliary-LLM-driven `specify` autocomplete. The CLI / tool surface lands with a stub that returns a structured "spec needed" template; the LLM call wire-up can be a 50-line follow-up once the team picks which model to use for the auxiliary role.
- Workspace subprocess spawning (`scratch`, `dir:<path>`, `worktree`). Hermes spawns a full subprocess per worker; rantaiclaw's "workers" today are the agent runtime itself. We persist `workspace_kind`/`workspace_path` in the schema and expose the env vars the dispatcher would inject, but the actual subprocess spawn is a TODO with a clear extension point. The dispatcher loop still does claim/heartbeat/reclaim correctly so when spawn is wired up, the rest is ready.

These three are explicitly called out as "phase-2" in the PR description with non-handwavy reasons.

## Architecture map (Hermes Python → RantaiClaw Rust)

| Hermes | RantaiClaw | Notes |
|---|---|---|
| `hermes_cli/kanban_db.py` (~4800 LoC) | `src/kanban/store.rs` + `src/kanban/schema.rs` + `src/kanban/boards.rs` + `src/kanban/runs.rs` + `src/kanban/notify.rs` | Split for module SRP; SQL roughly 1:1. |
| `hermes_cli/kanban.py` (~2200 LoC) | `src/kanban/cli.rs` + `src/main.rs` clap subtree | clap subcommand `rantaiclaw kanban …`. |
| `tools/kanban_tools.py` (~1100 LoC) | `src/tools/kanban_*.rs` (one tool per Hermes tool) | Registered in `src/tools/mod.rs`. |
| Slash commands `/kanban` | `src/tui/commands/kanban.rs` | Routed in `src/tui/commands/mod.rs`. |
| Gateway dispatcher (embedded) | `src/kanban/dispatcher.rs` + wire into `src/gateway/mod.rs` | Tokio task that ticks every 60s. |
| Gateway notifier bridge | `src/kanban/notifier.rs` + wire into `src/gateway/task_handlers.rs` | Tails `task_events` and emits to subscribed channels. |
| React Kanban plugin | not in this PR | Schema/REST hooks reserved. |
| `hermes_cli/kanban_specify.py` | `src/kanban/specify.rs` | Aux-LLM stub. |
| Bundled skills `kanban-worker`, `kanban-orchestrator` | `docs/kanban/skills/{worker,orchestrator}.md` | Pure docs at this stage. |

## Module layout

```
src/kanban/
  mod.rs            -- pub use
  schema.rs         -- SQL DDL + migration
  store.rs          -- connection helpers, CRUD kernel
  boards.rs         -- multi-board (slug, dirs, switch)
  runs.rs           -- task_runs CRUD + end_run helpers
  notify.rs         -- kanban_notify_subs CRUD
  events.rs         -- event kinds + payload structs
  context.rs        -- build_worker_context (handoff text)
  dispatcher.rs     -- tick loop (promote, claim, reclaim, spawn-stub)
  notifier.rs       -- gateway tail & deliver
  specify.rs        -- triage specifier stub
  cli.rs            -- KanbanCommand enum + handle_command
  slash.rs          -- /kanban parser + run
  errors.rs         -- error types
  tests.rs          -- unit tests

src/tools/
  kanban_show.rs
  kanban_list.rs
  kanban_complete.rs
  kanban_block.rs
  kanban_heartbeat.rs
  kanban_comment.rs
  kanban_create.rs
  kanban_link.rs
  kanban_unblock.rs
```

## Schema (1:1 with Hermes v1)

```sql
CREATE TABLE tasks (
    id                   TEXT PRIMARY KEY,
    title                TEXT NOT NULL,
    body                 TEXT,
    assignee             TEXT,
    status               TEXT NOT NULL,          -- triage|todo|ready|running|blocked|done|archived
    priority             INTEGER DEFAULT 0,
    created_by           TEXT,
    created_at           INTEGER NOT NULL,
    started_at           INTEGER,
    completed_at         INTEGER,
    workspace_kind       TEXT NOT NULL DEFAULT 'scratch',
    workspace_path       TEXT,
    claim_lock           TEXT,
    claim_expires        INTEGER,
    tenant               TEXT,
    result               TEXT,
    idempotency_key      TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    worker_pid           INTEGER,
    last_failure_error   TEXT,
    max_runtime_seconds  INTEGER,
    last_heartbeat_at    INTEGER,
    current_run_id       INTEGER,
    workflow_template_id TEXT,
    current_step_key     TEXT,
    skills               TEXT,                   -- JSON array
    max_retries          INTEGER
);
CREATE TABLE task_links (parent_id TEXT, child_id TEXT, PRIMARY KEY(parent_id, child_id));
CREATE TABLE task_comments (id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT, author TEXT, body TEXT, created_at INTEGER);
CREATE TABLE task_events (id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT, run_id INTEGER, kind TEXT, payload TEXT, created_at INTEGER);
CREATE TABLE task_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL, profile TEXT, step_key TEXT,
    status TEXT NOT NULL, claim_lock TEXT, claim_expires INTEGER,
    worker_pid INTEGER, max_runtime_seconds INTEGER, last_heartbeat_at INTEGER,
    started_at INTEGER NOT NULL, ended_at INTEGER, outcome TEXT,
    summary TEXT, metadata TEXT, error TEXT
);
CREATE TABLE kanban_notify_subs (
    task_id TEXT, platform TEXT, chat_id TEXT, thread_id TEXT DEFAULT '',
    user_id TEXT, notifier_profile TEXT, created_at INTEGER, last_event_id INTEGER DEFAULT 0,
    PRIMARY KEY (task_id, platform, chat_id, thread_id)
);
```

Indexes match Hermes. WAL mode + `BEGIN IMMEDIATE` for write txns.

## Paths

```
$RANTAICLAW_HOME/                          (env: RANTAICLAW_HOME, default: ~/.rantaiclaw)
├── kanban.db                              (board="default", back-compat shape)
└── kanban/
    ├── current                            (one-line slug of active board)
    └── boards/
        ├── default/                       (metadata only; DB stays at root for back-compat)
        │   ├── board.json
        │   ├── workspaces/
        │   └── logs/
        └── <slug>/
            ├── kanban.db
            ├── board.json
            ├── workspaces/
            └── logs/
```

Env overrides (parity with Hermes, RANTAICLAW_ prefix):
- `RANTAICLAW_KANBAN_HOME` — pin umbrella root
- `RANTAICLAW_KANBAN_DB` — pin DB file path
- `RANTAICLAW_KANBAN_BOARD` — pin active board slug (set by dispatcher on worker spawn)
- `RANTAICLAW_KANBAN_TASK` — pin worker's task id (set by dispatcher on worker spawn; flips on tool surface)
- `RANTAICLAW_KANBAN_WORKSPACES_ROOT` — pin workspace root
- `RANTAICLAW_TENANT` — current tenant

## CLI surface (parity with Hermes)

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
rantaiclaw kanban log <id> [--tail BYTES]
rantaiclaw kanban notify-subscribe <id> --platform P --chat-id C [--thread-id T] [--user-id U]
rantaiclaw kanban notify-list [<id>] [--json]
rantaiclaw kanban notify-unsubscribe <id> --platform P --chat-id C [--thread-id T]
rantaiclaw kanban context <id>
rantaiclaw kanban specify [<id> | --all] [--tenant T] [--author NAME] [--json]
rantaiclaw kanban gc [--event-retention-days N] [--log-retention-days N]
rantaiclaw kanban boards list
rantaiclaw kanban boards create <slug> [--name] [--description] [--icon] [--switch]
rantaiclaw kanban boards switch <slug>
rantaiclaw kanban boards show
rantaiclaw kanban boards rename <slug> "<display name>"
rantaiclaw kanban boards rm <slug> [--delete]
rantaiclaw kanban --board <slug> <any-subcommand>           # one-shot board scope
```

## Slash commands (`/kanban …` in TUI)

Same surface as CLI, parsed by `src/kanban/slash.rs` via `shlex` and dispatched to the same handlers.

## Agent tools (in `src/tools/`)

| Tool name | Function | When schema is active |
|---|---|---|
| `kanban_show` | Read current task | `RANTAICLAW_KANBAN_TASK` env set OR profile has `kanban` toolset |
| `kanban_list` | List tasks with filters | orchestrator |
| `kanban_complete` | Close task with summary/metadata | worker or orchestrator |
| `kanban_block` | Block on human input | worker or orchestrator |
| `kanban_heartbeat` | Liveness signal | worker |
| `kanban_comment` | Append comment | worker or orchestrator |
| `kanban_create` | Spawn child task | orchestrator |
| `kanban_link` | Add dep edge | orchestrator |
| `kanban_unblock` | Move blocked → ready | orchestrator |

`check_fn` parity: each tool's `is_available()` returns true only when the env var is set or the active config opted in.

## Dispatcher loop (in-process)

`src/kanban/dispatcher.rs` exposes `DispatcherHandle::spawn(config, board?)` that owns a tokio task running this loop at `dispatch_interval_seconds` (config-controlled, default 60):

1. `release_stale_claims()` — TTL-expired claims with dead PID → reclaimed; alive PID → claim extended.
2. `detect_crashed_workers()` — PID gone but TTL not expired → crashed + failure counter ++.
3. `enforce_max_runtime()` — runtime cap exceeded → SIGTERM (then SIGKILL after grace).
4. `recompute_ready()` — promote `todo` → `ready` when all parents `done`.
5. `claim_ready_tasks(max)` — atomically claim and (when worker spawn is wired) spawn `max` workers.
6. After `failure_limit` consecutive failures → auto-block with the last error.

Gateway integration: `src/gateway/mod.rs` constructs the handle on boot when `kanban.dispatch_in_gateway` is true.

## Notifier bridge

`src/kanban/notifier.rs`:
- Polls `task_events` every few seconds, joins with `kanban_notify_subs` by `task_id`.
- For terminal events (`completed`, `blocked`, `gave_up`, `crashed`, `timed_out`) and unseen `last_event_id`s, calls into rantaiclaw's existing channel system to send a one-line summary back to the originating chat.
- Auto-deletes the subscription when the task hits `done` or `archived`.

## Config knobs

`src/config/schema.rs` gains a `kanban` block:

```toml
[kanban]
enabled = true
dispatch_in_gateway = true
dispatch_interval_seconds = 60
failure_limit = 2
claim_ttl_seconds = 900               # 15m
home = "~/.rantaiclaw"                # optional override

[kanban.dashboard]
default_tenant = ""
lane_by_profile = false
include_archived_by_default = false
render_markdown = true
```

## TUI integration

Add a `Kanban` view to the existing TUI app:
- Toggleable from main view (key `k`).
- Columns: triage, todo, ready, running, blocked, done. Counts in header.
- Up/down to navigate cards within a column, left/right between columns.
- Enter to open task detail (title, body, comments, events).
- `c` to comment, `a` to archive, `b` to block, `u` to unblock.
- Live refresh from `task_events` (poll every 2s).

Implementation: new file `src/tui/widgets/kanban.rs` + new mode in `src/tui/app.rs`.

## Worker context

`src/kanban/context.rs` builds the same `worker_context` string Hermes does:
- Title + body (truncated to 8 KB)
- Parent handoffs (most recent completed run's summary + metadata per parent)
- Prior attempts on this task (most recent 10)
- Full comment thread (most recent 30, each truncated to 2 KB)
- Field bytes capped at 4 KB

## Testing strategy

- Unit tests per module (`src/kanban/tests.rs` + `#[cfg(test)] mod tests` in each file).
- Schema migration test: open old DB shape (Hermes-compatible), verify additive columns are added.
- Concurrency test: spawn N tasks, race two writers for the same `ready` task, assert only one wins.
- End-to-end CLI test: invoke `rantaiclaw kanban create` then `list`/`show`/`complete` via `assert_cmd`.
- Tool wiring test: assert `kanban_show` is registered, schema correct, only enabled when env set.
- Dispatcher test: create task with no parents, run one tick, observe `ready` → `running` transition.
- Notifier test: subscribe, fire `completed`, assert message reaches the channel stub.

Verification commands:
```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --features tui kanban
```

## Risk + rollback

- **Risk**: schema mistake locks user data into an irreversible shape. Mitigation: initial migration is purely additive; rusqlite + `IF NOT EXISTS`.
- **Risk**: dispatcher races with worker writes. Mitigation: CAS UPDATEs, WAL, `BEGIN IMMEDIATE`. Identical to Hermes.
- **Risk**: feature surface too large for a single PR review. Mitigation: code is in one new top-level module, no edits to existing critical paths beyond clap subcommand wiring + tool registration.
- **Rollback**: revert the merge commit; the only user-facing artifact created on first use is `~/.rantaiclaw/kanban.db`, which is dormant once the binary is gone.

## Build sequence

1. Module skeleton + schema + connection.
2. Kernel CRUD + events (create / get / list / comment / link / archive).
3. Lifecycle (claim / heartbeat / complete / block / unblock / specify).
4. Runs (insert on claim, close on terminal, synthesize on never-claimed).
5. Boards multi-project (paths, current, list/create/switch/rename/rm).
6. Notify subs.
7. Worker context.
8. Dispatcher loop.
9. CLI commands (clap subtree + handlers).
10. Slash commands.
11. Agent tools (one per Hermes tool).
12. TUI view.
13. Tests.
14. Docs (`docs/kanban.md`, skill stubs, runtime-contract refs).
15. Validate, commit, PR.
