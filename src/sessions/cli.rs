//! CLI handlers for session inspection (mirrors TUI `/sessions /resume /search /title /insights`).
//!
//! Headless equivalents so automated tests, agents, and operators can drive
//! the session subsystem without a TTY.

use anyhow::{bail, Result};
use chrono::{TimeZone, Utc};

use super::{SessionRef, SessionStore};

/// Resolve the same sessions.db path the TUI opens — the active profile's
/// `profiles/<name>/sessions/sessions.db`.
pub fn open_store() -> Result<SessionStore> {
    let path = crate::profile::ProfileManager::active()?.sessions_db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    SessionStore::open(&path)
}

fn fmt_ts(ts: i64) -> String {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

pub fn list(limit: usize, source: Option<&str>) -> Result<()> {
    let store = open_store()?;
    // The default `list` excludes `source='channel'` rows so the operator's
    // routine scan does not surface transcripts that already live in their
    // own place. Pass `--source channel` (and friends) to view them.
    let sessions = match source {
        Some(src) => store.list_sessions_paged_with_source(limit, 0, Some(src))?,
        None => store.list_sessions_paged_visible(limit, 0)?,
    };
    if sessions.is_empty() {
        match source {
            Some(_) => println!("No sessions match this filter."),
            None => println!("No sessions yet."),
        }
        return Ok(());
    }
    crate::cli_style::section(&format!("sessions ({})", sessions.len()));
    for s in &sessions {
        let short = s.id.chars().take(8).collect::<String>();
        let title = s.title.as_deref().unwrap_or("(untitled)");
        println!("  {short}  {title}");
        println!(
            "            {}",
            crate::cli_style::dim(&format!(
                "{}  ·  {} msgs  ·  {}  ·  {}",
                fmt_ts(s.started_at),
                s.message_count,
                s.model,
                s.source
            ))
        );
    }
    Ok(())
}

pub fn get(id_prefix: &str, limit: usize) -> Result<()> {
    let store = open_store()?;
    let session_id = resolve_id(&store, id_prefix)?;
    let session = store
        .get_session(&session_id)?
        .ok_or_else(|| anyhow::anyhow!("no session matches prefix `{id_prefix}`"))?;
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
        let short = r.session_id.chars().take(8).collect::<String>();
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
    let session_id = resolve_id(&store, id_prefix)?;
    store.set_title(&session_id, title)?;
    println!("Title set on {session_id}: {title}");
    Ok(())
}

/// Resolve a full session id or a unique prefix into a concrete id.
///
/// Resolution runs in SQL across the whole table. It used to scan the 500 most
/// recent sessions in memory, which made anything older unaddressable even by
/// its full id, and checked uniqueness only within that window.
fn resolve_id(store: &SessionStore, id_prefix: &str) -> Result<String> {
    match store.resolve_id(id_prefix)? {
        SessionRef::One(id) => Ok(id),
        SessionRef::None => bail!("no session matches prefix `{id_prefix}`"),
        SessionRef::Ambiguous(n) => {
            bail!("`{id_prefix}` is ambiguous ({n} matches); use a longer prefix")
        }
    }
}

/// Resolve a `chat --resume` argument to the full id of an existing session.
///
/// Refuses, before anything is opened or written, with the same wording
/// `session get` uses, so a typo never reaches the TUI as a bound id.
pub fn resolve_session_for_resume(store: &SessionStore, id_or_prefix: &str) -> Result<String> {
    resolve_id(store, id_or_prefix)
}

/// Cumulative session/message stats — the headless equivalent of TUI `/insights`.
pub fn insights() -> Result<()> {
    let store = open_store()?;
    // Aggregate in SQL so totals stay correct past any page size.
    let stats = store.stats()?;
    let avg = if stats.total_sessions > 0 {
        stats.total_messages as f64 / stats.total_sessions as f64
    } else {
        0.0
    };
    const W: usize = 11;
    crate::cli_style::section("insights");
    crate::cli_style::field("Sessions", W, &stats.total_sessions.to_string());
    crate::cli_style::field("Messages", W, &stats.total_messages.to_string());
    crate::cli_style::field("Avg/session", W, &format!("{avg:.1}"));
    if let (Some(id), Some(started)) = (&stats.latest_session_id, stats.latest_session_started_at) {
        crate::cli_style::field(
            "Latest",
            W,
            &format!(
                "{}  ·  {}",
                id.chars().take(8).collect::<String>(),
                fmt_ts(started)
            ),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file-backed store holding one `sessions` row per id. The rows are
    /// written through a second connection, since the store's `conn` is private.
    fn store_with(dir: &std::path::Path, ids: &[&str]) -> SessionStore {
        let path = dir.join("sessions.db");
        let store = SessionStore::open(&path).expect("open store");
        let raw = rusqlite::Connection::open(&path).expect("raw connection");
        for id in ids {
            raw.execute(
                "INSERT INTO sessions (id, model, started_at, source) \
                 VALUES (?1, 'm', 0, 'tui')",
                rusqlite::params![id],
            )
            .expect("insert session");
        }
        store
    }

    #[test]
    fn resume_binds_the_full_id_of_a_unique_prefix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let full = "12176d39-aaaa-4bbb-8ccc-0123456789ab";
        let store = store_with(dir.path(), &[full, "99999999-aaaa-4bbb-8ccc-0123456789ab"]);
        assert_eq!(
            resolve_session_for_resume(&store, "12176d39").unwrap(),
            full
        );
    }

    #[test]
    fn resume_refuses_an_unknown_id_with_the_session_get_wording() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_with(dir.path(), &["12176d39-aaaa-4bbb-8ccc-0123456789ab"]);
        let err = resolve_session_for_resume(&store, "00000000-nope").unwrap_err();
        assert_eq!(err.to_string(), "no session matches prefix `00000000-nope`");
    }

    #[test]
    fn resume_refuses_an_ambiguous_prefix_with_the_session_get_wording() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_with(
            dir.path(),
            &[
                "12176d39-aaaa-4bbb-8ccc-0123456789ab",
                "12176d39-bbbb-4bbb-8ccc-0123456789ab",
            ],
        );
        let err = resolve_session_for_resume(&store, "12176d39").unwrap_err();
        assert_eq!(
            err.to_string(),
            "`12176d39` is ambiguous (2 matches); use a longer prefix"
        );
    }

    #[test]
    fn resume_refuses_multibyte_garbage_without_panicking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_with(dir.path(), &["12176d39-aaaa-4bbb-8ccc-0123456789ab"]);
        let err = resolve_session_for_resume(&store, "日本語日本語").unwrap_err();
        assert_eq!(err.to_string(), "no session matches prefix `日本語日本語`");
    }

    #[test]
    fn resume_refusal_writes_nothing_to_the_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_with(dir.path(), &[]);
        assert!(resolve_session_for_resume(&store, "00000000-nope").is_err());
        assert_eq!(store.count_sessions().unwrap(), 0);
    }
}
