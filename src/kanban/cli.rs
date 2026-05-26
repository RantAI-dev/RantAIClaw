//! `rantaiclaw kanban …` clap subcommand surface — parity with Hermes
//! `hermes kanban …` so existing playbooks transfer 1:1.
//!
//! Every handler accepts a `&mut dyn Write` so the same code path serves both
//! the binary CLI (writing to stdout) and the `/kanban` slash command (writing
//! to a buffer the channel adapter delivers as a message).

use std::io::Write;

use clap::{Args, Subcommand};
use serde_json::json;

use crate::kanban::boards::{
    create_board, get_current_board, list_boards, remove_board, rename_board, set_current_board,
    DEFAULT_BOARD,
};
use crate::kanban::context::build_worker_context;
use crate::kanban::dispatcher::{Dispatcher, DispatcherOptions};
use crate::kanban::errors::{KanbanError, Result};
use crate::kanban::notify::{subscribe, unsubscribe, SubscribeInput};
use crate::kanban::runs::list_runs;
use crate::kanban::specify::{specify_all, specify_triage, TemplateSpecifier};
use crate::kanban::store::{
    add_comment, add_link, archive_task, assign_task, block_task, claim_task, complete_task,
    connect, create_task, get_task, heartbeat_claim, init_db, list_comments, list_events,
    list_tasks, remove_link, unblock_task, CreateTaskInput, ListFilter,
};

#[derive(Subcommand, Debug, Clone)]
pub enum KanbanCommand {
    /// Create the kanban DB.
    Init,
    /// Create a new task.
    Create(CreateArgs),
    /// List tasks.
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Show a task.
    Show(ShowArgs),
    /// (Re)assign a task — pass 'none' to unassign.
    Assign { id: String, profile: String },
    /// Add a parent → child dependency.
    Link { parent_id: String, child_id: String },
    /// Remove a dependency.
    Unlink { parent_id: String, child_id: String },
    /// Atomically claim a `ready` task.
    Claim {
        id: String,
        #[arg(long)]
        ttl: Option<i64>,
    },
    /// Append a comment to a task.
    Comment {
        id: String,
        text: String,
        #[arg(long)]
        author: Option<String>,
    },
    /// Complete one or more tasks.
    Complete(CompleteArgs),
    /// Block a task awaiting human input.
    Block {
        id: String,
        reason: String,
        #[arg(long, value_delimiter = ' ')]
        ids: Vec<String>,
    },
    /// Move blocked tasks back to ready.
    Unblock { ids: Vec<String> },
    /// Archive one or more tasks.
    Archive { ids: Vec<String> },
    /// Follow a single task's event stream (blocks).
    Tail { id: String },
    /// Heartbeat a long-running worker's claim.
    Heartbeat {
        id: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// One-shot dispatcher pass.
    Dispatch(DispatchArgs),
    /// Per-status + per-assignee counts.
    Stats {
        #[arg(long)]
        json: bool,
    },
    /// Attempt history for one task.
    Runs {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Print the handoff text the next worker would see.
    Context { id: String },
    /// Promote a triage task into a real spec; promote all with --all.
    Specify(SpecifyArgs),
    /// Subscribe a gateway chat to a task's terminal events.
    NotifySubscribe(NotifySubArgs),
    /// List active gateway subscriptions.
    NotifyList(NotifyListArgs),
    /// Remove a gateway subscription.
    NotifyUnsubscribe(NotifyUnsubArgs),
    /// Per-assignee task counts.
    Assignees {
        #[arg(long)]
        json: bool,
    },
    /// Manage boards (multi-project).
    Boards {
        #[command(subcommand)]
        cmd: BoardsCommand,
    },
    /// Live stream of board events (blocks).
    Watch(WatchArgs),
}

#[derive(Args, Debug, Clone)]
pub struct CreateArgs {
    pub title: String,
    #[arg(long)]
    pub body: Option<String>,
    #[arg(long)]
    pub assignee: Option<String>,
    #[arg(long = "parent", value_name = "ID")]
    pub parents: Vec<String>,
    #[arg(long)]
    pub tenant: Option<String>,
    /// scratch | worktree | dir:<path>
    #[arg(long, default_value = "scratch")]
    pub workspace: String,
    #[arg(long, default_value_t = 0)]
    pub priority: i64,
    #[arg(long)]
    pub triage: bool,
    #[arg(long)]
    pub idempotency_key: Option<String>,
    /// e.g. "30m", "2h", "1d", or raw seconds
    #[arg(long)]
    pub max_runtime: Option<String>,
    #[arg(long = "skill")]
    pub skills: Vec<String>,
    #[arg(long)]
    pub max_retries: Option<i64>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    #[arg(long)]
    pub mine: bool,
    #[arg(long)]
    pub assignee: Option<String>,
    #[arg(long)]
    pub status: Option<String>,
    #[arg(long)]
    pub tenant: Option<String>,
    #[arg(long)]
    pub archived: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ShowArgs {
    pub id: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CompleteArgs {
    pub ids: Vec<String>,
    #[arg(long)]
    pub result: Option<String>,
    #[arg(long)]
    pub summary: Option<String>,
    #[arg(long)]
    pub metadata: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct DispatchArgs {
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub max: Option<usize>,
    #[arg(long)]
    pub failure_limit: Option<u32>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct SpecifyArgs {
    pub id: Option<String>,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub tenant: Option<String>,
    #[arg(long)]
    pub author: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct NotifySubArgs {
    pub id: String,
    #[arg(long)]
    pub platform: String,
    #[arg(long = "chat-id")]
    pub chat_id: String,
    #[arg(long = "thread-id")]
    pub thread_id: Option<String>,
    #[arg(long = "user-id")]
    pub user_id: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct NotifyListArgs {
    pub id: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug, Clone)]
pub struct NotifyUnsubArgs {
    pub id: String,
    #[arg(long)]
    pub platform: String,
    #[arg(long = "chat-id")]
    pub chat_id: String,
    #[arg(long = "thread-id")]
    pub thread_id: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct WatchArgs {
    #[arg(long)]
    pub assignee: Option<String>,
    #[arg(long)]
    pub tenant: Option<String>,
    /// Comma-separated event kinds.
    #[arg(long)]
    pub kinds: Option<String>,
    #[arg(long, default_value_t = 2)]
    pub interval: u64,
}

#[derive(Subcommand, Debug, Clone)]
pub enum BoardsCommand {
    /// List boards.
    List,
    /// Create a new board.
    Create {
        slug: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        icon: Option<String>,
        #[arg(long)]
        switch: bool,
    },
    /// Switch the active board.
    Switch { slug: String },
    /// Print the active board slug.
    Show,
    /// Rename a board's display name (slug is immutable).
    Rename { slug: String, name: String },
    /// Archive (default) or hard-delete a board.
    Rm {
        slug: String,
        #[arg(long)]
        delete: bool,
    },
}

/// Top-level entry. The binary calls this with `&mut std::io::stdout()`; the
/// slash command writes into a `Vec<u8>` and delivers the rendered string as
/// a chat message.
pub fn handle_command<W: Write>(
    cmd: &KanbanCommand,
    board: Option<&str>,
    out: &mut W,
) -> Result<()> {
    match cmd {
        KanbanCommand::Init => {
            let path = init_db(board)?;
            writeln!(out, "kanban DB initialised at {}", path.display())?;
            Ok(())
        }
        KanbanCommand::Create(args) => cmd_create(args, board, out),
        KanbanCommand::List(args) => cmd_list(args, board, out),
        KanbanCommand::Show(args) => cmd_show(args, board, out),
        KanbanCommand::Assign { id, profile } => cmd_assign(id, profile, board, out),
        KanbanCommand::Link {
            parent_id,
            child_id,
        } => {
            let conn = connect(board)?;
            let ok = add_link(&conn, parent_id, child_id)?;
            writeln!(
                out,
                "{} {} → {}",
                if ok {
                    "linked"
                } else {
                    "no-op (already linked or cycle)"
                },
                parent_id,
                child_id
            )?;
            Ok(())
        }
        KanbanCommand::Unlink {
            parent_id,
            child_id,
        } => {
            let conn = connect(board)?;
            let ok = remove_link(&conn, parent_id, child_id)?;
            writeln!(
                out,
                "{} {} ↛ {}",
                if ok { "unlinked" } else { "not linked" },
                parent_id,
                child_id
            )?;
            Ok(())
        }
        KanbanCommand::Claim { id, ttl } => {
            let conn = connect(board)?;
            let claimed = claim_task(&conn, id, *ttl, None)?;
            match claimed {
                Some(t) => {
                    writeln!(
                        out,
                        "claimed {} (lock={})",
                        t.id,
                        t.claim_lock.unwrap_or_default()
                    )?;
                    Ok(())
                }
                None => Err(KanbanError::InvalidStatus(
                    "claim refused — task not in ready or already claimed".into(),
                )),
            }
        }
        KanbanCommand::Comment { id, text, author } => {
            let conn = connect(board)?;
            let aid = add_comment(&conn, id, author.as_deref().unwrap_or("you"), text)?;
            writeln!(out, "comment #{aid} on {id}")?;
            Ok(())
        }
        KanbanCommand::Complete(args) => cmd_complete(args, board, out),
        KanbanCommand::Block { id, reason, ids } => {
            let conn = connect(board)?;
            let all: Vec<&String> = std::iter::once(id).chain(ids.iter()).collect();
            for tid in &all {
                let ok = block_task(&conn, tid, Some(reason))?;
                writeln!(out, "{} {tid}", if ok { "blocked" } else { "no-op" })?;
            }
            Ok(())
        }
        KanbanCommand::Unblock { ids } => {
            let conn = connect(board)?;
            for id in ids {
                let ok = unblock_task(&conn, id)?;
                writeln!(out, "{} {id}", if ok { "unblocked" } else { "no-op" })?;
            }
            Ok(())
        }
        KanbanCommand::Archive { ids } => {
            let conn = connect(board)?;
            for id in ids {
                let ok = archive_task(&conn, id)?;
                writeln!(out, "{} {id}", if ok { "archived" } else { "no-op" })?;
            }
            Ok(())
        }
        KanbanCommand::Tail { id } => cmd_tail(id, board, out),
        KanbanCommand::Heartbeat { id, note } => {
            let conn = connect(board)?;
            let ok = heartbeat_claim(&conn, id, None, None)?;
            writeln!(
                out,
                "{} {id}{}",
                if ok {
                    "heartbeat ok"
                } else {
                    "heartbeat lost claim"
                },
                note.as_deref()
                    .map(|n| format!(" — {n}"))
                    .unwrap_or_default()
            )?;
            Ok(())
        }
        KanbanCommand::Dispatch(args) => cmd_dispatch(args, board, out),
        KanbanCommand::Stats { json } => cmd_stats(*json, board, out),
        KanbanCommand::Runs { id, json } => cmd_runs(id, *json, board, out),
        KanbanCommand::Context { id } => {
            let conn = connect(board)?;
            let ctx = build_worker_context(&conn, id)?;
            write!(out, "{ctx}")?;
            Ok(())
        }
        KanbanCommand::Specify(args) => cmd_specify(args, board, out),
        KanbanCommand::NotifySubscribe(args) => cmd_notify_subscribe(args, board, out),
        KanbanCommand::NotifyList(args) => cmd_notify_list(args, board, out),
        KanbanCommand::NotifyUnsubscribe(args) => cmd_notify_unsubscribe(args, board, out),
        KanbanCommand::Assignees { json } => cmd_assignees(*json, board, out),
        KanbanCommand::Watch(args) => cmd_watch(args, board, out),
        KanbanCommand::Boards { cmd } => cmd_boards(cmd, out),
    }
}

fn parse_max_runtime(input: Option<&str>) -> Option<i64> {
    let s = input?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(n) = s.parse::<i64>() {
        return Some(n);
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: i64 = num.parse().ok()?;
    Some(match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3_600,
        "d" => n * 86_400,
        _ => return None,
    })
}

fn cmd_create<W: Write>(args: &CreateArgs, board: Option<&str>, out: &mut W) -> Result<()> {
    let conn = connect(board)?;
    let (workspace_kind, workspace_path) = if let Some(rest) = args.workspace.strip_prefix("dir:") {
        ("dir".to_string(), Some(rest.to_string()))
    } else {
        (args.workspace.clone(), None)
    };
    let input = CreateTaskInput {
        title: args.title.clone(),
        body: args.body.clone(),
        assignee: args.assignee.clone(),
        created_by: None,
        workspace_kind: Some(workspace_kind),
        workspace_path,
        tenant: args.tenant.clone(),
        priority: Some(args.priority),
        parents: args.parents.clone(),
        triage: args.triage,
        idempotency_key: args.idempotency_key.clone(),
        max_runtime_seconds: parse_max_runtime(args.max_runtime.as_deref()),
        skills: if args.skills.is_empty() {
            None
        } else {
            Some(args.skills.clone())
        },
        max_retries: args.max_retries,
    };
    let id = create_task(&conn, &input)?;
    let task = get_task(&conn, &id)?.expect("just created");
    if args.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&task)?)?;
    } else {
        writeln!(
            out,
            "Created {} ({}, assignee={})",
            task.id,
            task.status,
            task.assignee.unwrap_or_else(|| "(unassigned)".into())
        )?;
    }
    Ok(())
}

fn cmd_list<W: Write>(args: &ListArgs, board: Option<&str>, out: &mut W) -> Result<()> {
    let conn = connect(board)?;
    let filter = ListFilter {
        assignee: if args.mine {
            std::env::var("USER").ok()
        } else {
            args.assignee.clone()
        },
        status: args.status.clone(),
        tenant: args.tenant.clone(),
        include_archived: args.archived,
        limit: None,
    };
    let tasks = list_tasks(&conn, &filter)?;
    if args.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&tasks)?)?;
        return Ok(());
    }
    if tasks.is_empty() {
        writeln!(out, "(no tasks)")?;
        return Ok(());
    }
    for t in tasks {
        writeln!(
            out,
            "{:<12} {:<8} {:<14} {}",
            t.id,
            t.status,
            t.assignee.unwrap_or_else(|| "-".into()),
            t.title
        )?;
    }
    Ok(())
}

fn cmd_show<W: Write>(args: &ShowArgs, board: Option<&str>, out: &mut W) -> Result<()> {
    let conn = connect(board)?;
    let task =
        get_task(&conn, &args.id)?.ok_or_else(|| KanbanError::UnknownTask(args.id.clone()))?;
    let comments = list_comments(&conn, &args.id)?;
    let events = list_events(&conn, &args.id)?;
    let runs = list_runs(&conn, &args.id)?;
    if args.json {
        let bundle = json!({
            "task": task,
            "comments": comments,
            "events": events,
            "runs": runs,
        });
        writeln!(out, "{}", serde_json::to_string_pretty(&bundle)?)?;
        return Ok(());
    }
    writeln!(out, "# {}  ({})", task.title, task.id)?;
    writeln!(
        out,
        "status={}  assignee={}  tenant={}",
        task.status,
        task.assignee.as_deref().unwrap_or("-"),
        task.tenant.as_deref().unwrap_or("-")
    )?;
    if let Some(body) = task.body.as_deref() {
        writeln!(out, "\n{body}")?;
    }
    if !comments.is_empty() {
        writeln!(out, "\n## Comments")?;
        for c in &comments {
            writeln!(out, "- {} @ {}: {}", c.author, c.created_at, c.body)?;
        }
    }
    if !runs.is_empty() {
        writeln!(out, "\n## Runs")?;
        for r in &runs {
            writeln!(
                out,
                "- #{} {} {}",
                r.id,
                r.outcome.as_deref().unwrap_or(&r.status),
                r.summary.as_deref().unwrap_or("")
            )?;
        }
    }
    if !events.is_empty() {
        writeln!(out, "\n## Events")?;
        let len = events.len();
        let skip = len.saturating_sub(20);
        for e in events.iter().skip(skip) {
            writeln!(out, "- #{} {} {}", e.id, e.kind, e.created_at)?;
        }
    }
    Ok(())
}

fn cmd_assign<W: Write>(id: &str, profile: &str, board: Option<&str>, out: &mut W) -> Result<()> {
    let conn = connect(board)?;
    let prof = if profile.eq_ignore_ascii_case("none") {
        None
    } else {
        Some(profile)
    };
    let ok = assign_task(&conn, id, prof)?;
    writeln!(
        out,
        "{} {id} → {}",
        if ok {
            "assigned"
        } else {
            "no-op (task running or missing)"
        },
        prof.unwrap_or("none")
    )?;
    Ok(())
}

fn cmd_complete<W: Write>(args: &CompleteArgs, board: Option<&str>, out: &mut W) -> Result<()> {
    if args.summary.is_some() && args.ids.len() > 1 {
        return Err(KanbanError::InvalidStatus(
            "bulk --summary refused; structured handoff is per-run, run one task at a time".into(),
        ));
    }
    let conn = connect(board)?;
    let metadata: Option<serde_json::Value> = match args.metadata.as_deref() {
        Some(s) => Some(serde_json::from_str(s)?),
        None => None,
    };
    for id in &args.ids {
        let ok = complete_task(
            &conn,
            id,
            args.result.as_deref(),
            args.summary.as_deref(),
            metadata.as_ref(),
        )?;
        writeln!(out, "{} {id}", if ok { "completed" } else { "no-op" })?;
    }
    Ok(())
}

fn cmd_tail<W: Write>(id: &str, board: Option<&str>, out: &mut W) -> Result<()> {
    let conn = connect(board)?;
    let mut last_seen: i64 = 0;
    loop {
        let events = list_events(&conn, id)?;
        let fresh: Vec<_> = events.into_iter().filter(|e| e.id > last_seen).collect();
        for e in fresh {
            writeln!(
                out,
                "#{:<6} {} {} {}",
                e.id,
                e.created_at,
                e.kind,
                e.payload.as_ref().map_or(String::new(), |v| v.to_string())
            )?;
            last_seen = e.id;
        }
        out.flush().ok();
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

fn cmd_watch<W: Write>(args: &WatchArgs, board: Option<&str>, out: &mut W) -> Result<()> {
    let conn = connect(board)?;
    let kinds: Option<Vec<String>> = args
        .kinds
        .as_deref()
        .map(|s| s.split(',').map(str::trim).map(str::to_string).collect());
    let mut last_seen: i64 = 0;
    loop {
        let sql = "SELECT id, task_id, kind, payload, created_at FROM task_events \
                   WHERE id > ? ORDER BY id ASC";
        let mut stmt = conn.prepare(sql)?;
        let events: Vec<(i64, String, String, Option<String>, i64)> = stmt
            .query_map([last_seen], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .filter_map(std::result::Result::ok)
            .collect();
        for (eid, tid, kind, payload, created) in events {
            if let Some(ks) = &kinds {
                if !ks.contains(&kind) {
                    last_seen = eid;
                    continue;
                }
            }
            let task = get_task(&conn, &tid)?;
            let assignee = task
                .as_ref()
                .and_then(|t| t.assignee.clone())
                .unwrap_or_default();
            let tenant = task
                .as_ref()
                .and_then(|t| t.tenant.clone())
                .unwrap_or_default();
            if let Some(a) = &args.assignee {
                if &assignee != a {
                    last_seen = eid;
                    continue;
                }
            }
            if let Some(t) = &args.tenant {
                if &tenant != t {
                    last_seen = eid;
                    continue;
                }
            }
            writeln!(
                out,
                "#{eid:<6} {created} {tid} {kind} {}",
                payload.as_deref().unwrap_or("")
            )?;
            last_seen = eid;
        }
        out.flush().ok();
        std::thread::sleep(std::time::Duration::from_secs(args.interval));
    }
}

fn cmd_dispatch<W: Write>(args: &DispatchArgs, board: Option<&str>, out: &mut W) -> Result<()> {
    let mut opts = DispatcherOptions {
        board: board.map(str::to_string),
        ..Default::default()
    };
    if let Some(m) = args.max {
        opts.max_claims_per_tick = m;
    }
    if let Some(f) = args.failure_limit {
        opts.failure_limit = f;
    }
    let dispatcher = Dispatcher::new(opts);
    if args.dry_run {
        writeln!(out, "(dry-run) dispatcher options applied; no claims made")?;
        return Ok(());
    }
    let report = dispatcher.tick()?;
    if args.json {
        let v = json!({
            "reclaimed": report.reclaimed,
            "promoted": report.promoted,
            "claimed": report.claimed,
        });
        writeln!(out, "{v}")?;
    } else {
        writeln!(
            out,
            "reclaimed={} promoted={} claimed={}",
            report.reclaimed, report.promoted, report.claimed
        )?;
    }
    Ok(())
}

fn cmd_stats<W: Write>(as_json: bool, board: Option<&str>, out: &mut W) -> Result<()> {
    let conn = connect(board)?;
    let mut stmt = conn.prepare("SELECT status, COUNT(*) FROM tasks GROUP BY status")?;
    let by_status: Vec<(String, i64)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .filter_map(std::result::Result::ok)
        .collect();
    let mut stmt = conn.prepare(
        "SELECT COALESCE(assignee, '(unassigned)'), COUNT(*) FROM tasks GROUP BY assignee",
    )?;
    let by_assignee: Vec<(String, i64)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .filter_map(std::result::Result::ok)
        .collect();
    if as_json {
        let v = json!({"by_status": by_status, "by_assignee": by_assignee});
        writeln!(out, "{v}")?;
    } else {
        writeln!(out, "By status:")?;
        for (s, n) in by_status {
            writeln!(out, "  {s:<10} {n}")?;
        }
        writeln!(out, "By assignee:")?;
        for (a, n) in by_assignee {
            writeln!(out, "  {a:<24} {n}")?;
        }
    }
    Ok(())
}

fn cmd_runs<W: Write>(id: &str, as_json: bool, board: Option<&str>, out: &mut W) -> Result<()> {
    let conn = connect(board)?;
    let runs = list_runs(&conn, id)?;
    if as_json {
        writeln!(out, "{}", serde_json::to_string_pretty(&runs)?)?;
        return Ok(());
    }
    for r in runs {
        writeln!(
            out,
            "#{:<4} {:<10} {:<12} {}",
            r.id,
            r.outcome.unwrap_or(r.status),
            r.profile.unwrap_or_default(),
            r.summary.unwrap_or_default()
        )?;
    }
    Ok(())
}

fn cmd_specify<W: Write>(args: &SpecifyArgs, board: Option<&str>, out: &mut W) -> Result<()> {
    let conn = connect(board)?;
    let specifier = TemplateSpecifier;
    if args.all {
        let outcomes = specify_all(
            &conn,
            &specifier,
            args.tenant.as_deref(),
            args.author.as_deref(),
        )?;
        if args.json {
            let v: Vec<_> = outcomes
                .iter()
                .map(|o| json!({"task_id": o.task_id, "ok": o.ok, "reason": o.reason}))
                .collect();
            writeln!(out, "{}", serde_json::to_string_pretty(&v)?)?;
        } else {
            for o in outcomes {
                writeln!(out, "{} {}", if o.ok { "✓" } else { "·" }, o.task_id)?;
            }
        }
        Ok(())
    } else if let Some(id) = args.id.as_deref() {
        let outcome = specify_triage(&conn, id, &specifier, args.author.as_deref())?;
        if args.json {
            writeln!(
                out,
                "{}",
                json!({"task_id": outcome.task_id, "ok": outcome.ok, "reason": outcome.reason})
            )?;
        } else {
            writeln!(
                out,
                "{} {}{}",
                if outcome.ok { "✓" } else { "·" },
                outcome.task_id,
                outcome
                    .reason
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default()
            )?;
        }
        Ok(())
    } else {
        Err(KanbanError::InvalidStatus(
            "specify requires <id> or --all".into(),
        ))
    }
}

fn cmd_notify_subscribe<W: Write>(
    args: &NotifySubArgs,
    board: Option<&str>,
    out: &mut W,
) -> Result<()> {
    let conn = connect(board)?;
    subscribe(
        &conn,
        &SubscribeInput {
            task_id: &args.id,
            platform: &args.platform,
            chat_id: &args.chat_id,
            thread_id: args.thread_id.as_deref(),
            user_id: args.user_id.as_deref(),
            notifier_profile: None,
        },
    )?;
    writeln!(out, "subscribed {}", args.id)?;
    Ok(())
}

fn cmd_notify_list<W: Write>(
    args: &NotifyListArgs,
    board: Option<&str>,
    out: &mut W,
) -> Result<()> {
    let conn = connect(board)?;
    let subs = crate::kanban::notify::list_subscriptions(&conn, args.id.as_deref())?;
    if args.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&subs)?)?;
    } else {
        for s in subs {
            writeln!(
                out,
                "{:<14} platform={} chat={} thread={}",
                s.task_id, s.platform, s.chat_id, s.thread_id
            )?;
        }
    }
    Ok(())
}

fn cmd_notify_unsubscribe<W: Write>(
    args: &NotifyUnsubArgs,
    board: Option<&str>,
    out: &mut W,
) -> Result<()> {
    let conn = connect(board)?;
    let ok = unsubscribe(
        &conn,
        &args.id,
        &args.platform,
        &args.chat_id,
        args.thread_id.as_deref(),
    )?;
    writeln!(
        out,
        "{} {}",
        if ok { "unsubscribed" } else { "no-op" },
        args.id
    )?;
    Ok(())
}

fn cmd_assignees<W: Write>(as_json: bool, board: Option<&str>, out: &mut W) -> Result<()> {
    let conn = connect(board)?;
    let mut stmt = conn.prepare(
        "SELECT COALESCE(assignee, '(unassigned)'), COUNT(*) FROM tasks GROUP BY assignee",
    )?;
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .filter_map(std::result::Result::ok)
        .collect();
    if as_json {
        writeln!(out, "{}", serde_json::to_string_pretty(&rows)?)?;
    } else {
        for (a, n) in rows {
            writeln!(out, "{a:<24} {n}")?;
        }
    }
    Ok(())
}

fn cmd_boards<W: Write>(cmd: &BoardsCommand, out: &mut W) -> Result<()> {
    match cmd {
        BoardsCommand::List => {
            let boards = list_boards(false)?;
            for b in boards {
                writeln!(
                    out,
                    "{}{:<24} {}",
                    if b.is_default { "* " } else { "  " },
                    b.slug,
                    b.name
                )?;
            }
            Ok(())
        }
        BoardsCommand::Create {
            slug,
            name,
            description,
            icon,
            switch,
        } => {
            let b = create_board(
                slug,
                name.as_deref(),
                description.as_deref(),
                icon.as_deref(),
            )?;
            writeln!(out, "created board {}", b.slug)?;
            if *switch {
                set_current_board(&b.slug)?;
                writeln!(out, "switched to {}", b.slug)?;
            }
            Ok(())
        }
        BoardsCommand::Switch { slug } => {
            if slug != DEFAULT_BOARD && !crate::kanban::boards::board_exists(Some(slug)) {
                return Err(KanbanError::UnknownBoard(slug.clone()));
            }
            set_current_board(slug)?;
            writeln!(out, "switched to {slug}")?;
            Ok(())
        }
        BoardsCommand::Show => {
            let active = get_current_board();
            writeln!(out, "active board: {active}")?;
            Ok(())
        }
        BoardsCommand::Rename { slug, name } => {
            let b = rename_board(slug, name)?;
            writeln!(out, "renamed {} → {}", b.slug, b.name)?;
            Ok(())
        }
        BoardsCommand::Rm { slug, delete } => {
            remove_board(slug, *delete)?;
            writeln!(
                out,
                "{} {slug}",
                if *delete { "deleted" } else { "archived" }
            )?;
            Ok(())
        }
    }
}
