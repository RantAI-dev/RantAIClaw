//! CLI handlers for session inspection (mirrors TUI `/sessions /resume /search /title /insights`).
//!
//! Headless equivalents so automated tests, agents, and operators can drive
//! the session subsystem without a TTY.

use anyhow::{bail, Result};
use chrono::{TimeZone, Utc};

use super::SessionStore;

/// Resolve the same sessions.db path the TUI opens.
fn open_store() -> Result<SessionStore> {
    let data_dir = directories::ProjectDirs::from("", "", "rantaiclaw")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from(".rantaiclaw"));
    std::fs::create_dir_all(&data_dir)?;
    SessionStore::open(&data_dir.join("sessions.db"))
}

fn fmt_ts(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn list(limit: usize) -> Result<()> {
    let store = open_store()?;
    let sessions = store.list_sessions(limit)?;
    if sessions.is_empty() {
        println!("No sessions yet.");
        return Ok(());
    }
    println!("Sessions ({}):", sessions.len());
    println!();
    for s in &sessions {
        let short = &s.id[..s.id.len().min(8)];
        let title = s.title.as_deref().unwrap_or("(untitled)");
        println!(
            "  {short}  {date}  {msgs:>4} msgs  {model}  {title}",
            date = fmt_ts(s.started_at),
            msgs = s.message_count,
            model = s.model,
        );
    }
    Ok(())
}

pub fn get(id_prefix: &str, limit: usize) -> Result<()> {
    let store = open_store()?;
    let sessions = store.list_sessions(500)?;
    let matched: Vec<_> = sessions
        .iter()
        .filter(|s| s.id.starts_with(id_prefix))
        .collect();
    let session = match matched.len() {
        0 => bail!("no session matches prefix `{id_prefix}`"),
        1 => matched[0],
        n => bail!("`{id_prefix}` is ambiguous ({n} matches); use a longer prefix"),
    };
    let messages = store.get_messages(&session.id)?;
    println!("Session {}", session.id);
    println!(
        "  title: {}",
        session.title.as_deref().unwrap_or("(untitled)")
    );
    println!("  model: {}", session.model);
    println!("  started: {}", fmt_ts(session.started_at));
    println!("  messages: {} (showing up to {limit})", messages.len());
    println!();
    for m in messages.iter().take(limit) {
        let preview: String = m.content.chars().take(200).collect();
        let suffix = if m.content.chars().count() > 200 {
            "…"
        } else {
            ""
        };
        println!("  [{role}] {ts}", role = m.role, ts = fmt_ts(m.timestamp));
        println!("    {preview}{suffix}");
        println!();
    }
    Ok(())
}

pub fn search(query: &str, limit: usize) -> Result<()> {
    let store = open_store()?;
    let results = store.search(query, limit)?;
    if results.is_empty() {
        println!("No matches for: {query}");
        return Ok(());
    }
    println!("{} match(es) for '{query}':", results.len());
    println!();
    for r in &results {
        let short = &r.session_id[..r.session_id.len().min(8)];
        let preview: String = r.content.chars().take(80).collect();
        let suffix = if r.content.chars().count() > 80 {
            "…"
        } else {
            ""
        };
        println!(
            "  [{short}] {ts}  {role:<9} {preview}{suffix}",
            ts = fmt_ts(r.timestamp),
            role = r.role,
        );
    }
    Ok(())
}

pub fn set_title(id_prefix: &str, title: &str) -> Result<()> {
    let store = open_store()?;
    let sessions = store.list_sessions(500)?;
    let matched: Vec<_> = sessions
        .iter()
        .filter(|s| s.id.starts_with(id_prefix))
        .collect();
    let session = match matched.len() {
        0 => bail!("no session matches prefix `{id_prefix}`"),
        1 => matched[0],
        n => bail!("`{id_prefix}` is ambiguous ({n} matches); use a longer prefix"),
    };
    store.set_title(&session.id, title)?;
    println!("Title set on {}: {title}", session.id);
    Ok(())
}

/// Cumulative session/message stats — the headless equivalent of TUI `/insights`.
pub fn insights() -> Result<()> {
    let store = open_store()?;
    let sessions = store.list_sessions(10_000)?;
    let total_sessions = sessions.len();
    let total_messages: i64 = sessions.iter().map(|s| s.message_count).sum();
    let avg = if total_sessions > 0 {
        total_messages as f64 / total_sessions as f64
    } else {
        0.0
    };
    println!("RantaiClaw Insights");
    println!("───────────────────");
    println!("  Sessions:         {total_sessions}");
    println!("  Messages:         {total_messages}");
    println!("  Avg msgs/session: {avg:.1}");
    if let Some(latest) = sessions.first() {
        println!("  Latest session:  {} ({})", &latest.id[..8], fmt_ts(latest.started_at));
    }
    Ok(())
}
