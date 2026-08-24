//! Per-session "Always"-granted tool names for the web console.
//!
//! Each SSE turn rebuilds a fresh [`ApprovalManager`](crate::approval::ApprovalManager),
//! so without this an "Always" grant would reset every message; keying grants by
//! the conversation's session id lets the grant persist across the conversation
//! (parity with the TUI's session-scoped allowlist).
//!
//! Process-scoped and bounded — the gateway is one process, and a convenience
//! grant is safe to drop under memory pressure (it just re-prompts). This lives
//! under `src/approval/` (not `src/gateway/`) so the tightening path in
//! `policy_writer` can revoke grants without `approval` depending on `gateway`
//! (CLAUDE.md §6.4 keeps the dependency direction inward to contracts).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use parking_lot::Mutex;

static SESSION_GRANTS: LazyLock<Mutex<HashMap<String, HashSet<String>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Cap on distinct sessions holding grants, so a long-lived gateway can't grow
/// the map without bound. A new session past the cap simply won't persist grants
/// (it re-prompts each turn — safe degradation).
const MAX_GRANT_SESSIONS: usize = 1000;

/// Tools this session has granted "Always" — used to seed a new turn's manager.
/// An empty/blank session id owns no grants.
pub fn session_granted_tools(session_id: &str) -> Vec<String> {
    if session_id.trim().is_empty() {
        return Vec::new();
    }
    SESSION_GRANTS
        .lock()
        .get(session_id)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

/// Merge a turn's "Always" grants into the session's persistent set. No-op when
/// the tool set or the session id is empty, and bounded: a brand-new session past
/// `MAX_GRANT_SESSIONS` is skipped rather than evicting an existing one.
pub fn record_session_grants<S: std::hash::BuildHasher>(
    session_id: &str,
    tools: &HashSet<String, S>,
) {
    if session_id.trim().is_empty() || tools.is_empty() {
        return;
    }
    let mut map = SESSION_GRANTS.lock();
    if !map.contains_key(session_id) && map.len() >= MAX_GRANT_SESSIONS {
        tracing::warn!(
            session_id = %session_id,
            cap = MAX_GRANT_SESSIONS,
            "web approval grant not persisted: session cap reached"
        );
        return;
    }
    map.entry(session_id.to_string())
        .or_default()
        .extend(tools.iter().cloned());
}

/// Drop one session's remembered "Always" grants. Called when a session is
/// deleted so its grants don't outlive it.
pub fn clear_session_grants(session_id: &str) {
    SESSION_GRANTS.lock().remove(session_id);
}

/// Drop every session's remembered "Always" grants. Called when the autonomy
/// policy changes so a tightening actually re-prompts instead of re-seeding a
/// stale blanket grant made under a looser preset.
pub fn clear_all_session_grants() {
    SESSION_GRANTS.lock().clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_session_id_is_never_keyed() {
        // A grant harvested for an empty session id must not create a "" bucket.
        record_session_grants("", &HashSet::from(["http_request".to_string()]));
        assert!(session_granted_tools("").is_empty());
    }

    #[test]
    fn grants_accumulate_across_turns() {
        let sid = "sess-grants-accumulate-9a1c";
        assert!(session_granted_tools(sid).is_empty());
        record_session_grants(sid, &HashSet::from(["http_request".to_string()]));
        record_session_grants(sid, &HashSet::new()); // empty is a no-op
        record_session_grants(sid, &HashSet::from(["browser".to_string()])); // accumulates
        let got: HashSet<String> = session_granted_tools(sid).into_iter().collect();
        assert_eq!(
            got,
            HashSet::from(["http_request".to_string(), "browser".to_string()])
        );
        clear_session_grants(sid);
    }

    #[test]
    fn clear_session_grants_empties_only_that_session() {
        let a = "sess-grants-clear-a-7b2d";
        let b = "sess-grants-clear-b-7b2d";
        record_session_grants(a, &HashSet::from(["browser".to_string()]));
        record_session_grants(b, &HashSet::from(["shell".to_string()]));
        clear_session_grants(a);
        assert!(session_granted_tools(a).is_empty());
        assert_eq!(session_granted_tools(b), vec!["shell".to_string()]);
        clear_session_grants(b);
    }
}
