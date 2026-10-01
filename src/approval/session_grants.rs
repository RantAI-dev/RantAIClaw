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

/// Cap on distinct sessions holding grants, so a long-lived gateway can't grow
/// the map without bound. A new session past the cap simply won't persist grants
/// (it re-prompts each turn — safe degradation).
const MAX_GRANT_SESSIONS: usize = 1000;

/// One map of session id to granted tool names. Production code reaches the
/// single [`SESSION_GRANTS`] instance through the free functions below; tests
/// build their own instance so a clear elsewhere in the process cannot reach
/// them.
struct SessionGrants {
    map: Mutex<HashMap<String, HashSet<String>>>,
}

impl SessionGrants {
    fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    fn granted_tools(&self, session_id: &str) -> Vec<String> {
        if session_id.trim().is_empty() {
            return Vec::new();
        }
        self.map
            .lock()
            .get(session_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn record<S: std::hash::BuildHasher>(&self, session_id: &str, tools: &HashSet<String, S>) {
        if session_id.trim().is_empty() || tools.is_empty() {
            return;
        }
        let mut map = self.map.lock();
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

    fn clear(&self, session_id: &str) {
        self.map.lock().remove(session_id);
    }

    fn clear_all(&self) {
        self.map.lock().clear();
    }
}

static SESSION_GRANTS: LazyLock<SessionGrants> = LazyLock::new(SessionGrants::new);

/// Tools this session has granted "Always" — used to seed a new turn's manager.
/// An empty/blank session id owns no grants.
pub fn session_granted_tools(session_id: &str) -> Vec<String> {
    SESSION_GRANTS.granted_tools(session_id)
}

/// Merge a turn's "Always" grants into the session's persistent set. No-op when
/// the tool set or the session id is empty, and bounded: a brand-new session past
/// `MAX_GRANT_SESSIONS` is skipped rather than evicting an existing one.
pub fn record_session_grants<S: std::hash::BuildHasher>(
    session_id: &str,
    tools: &HashSet<String, S>,
) {
    SESSION_GRANTS.record(session_id, tools);
}

/// Drop one session's remembered "Always" grants. Called when a session is
/// deleted so its grants don't outlive it.
pub fn clear_session_grants(session_id: &str) {
    SESSION_GRANTS.clear(session_id);
}

/// Drop every session's remembered "Always" grants. Called when the autonomy
/// policy changes so a tightening actually re-prompts instead of re-seeding a
/// stale blanket grant made under a looser preset.
pub fn clear_all_session_grants() {
    SESSION_GRANTS.clear_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_session_id_is_never_keyed() {
        // A grant harvested for an empty session id must not create a "" bucket.
        let grants = SessionGrants::new();
        grants.record("", &HashSet::from(["http_request".to_string()]));
        assert!(grants.granted_tools("").is_empty());
        assert!(grants.map.lock().is_empty());
    }

    #[test]
    fn grants_accumulate_across_turns() {
        let grants = SessionGrants::new();
        let sid = "sess-grants-accumulate-9a1c";
        assert!(grants.granted_tools(sid).is_empty());
        grants.record(sid, &HashSet::from(["http_request".to_string()]));
        grants.record(sid, &HashSet::new()); // empty is a no-op
        grants.record(sid, &HashSet::from(["browser".to_string()])); // accumulates
        let got: HashSet<String> = grants.granted_tools(sid).into_iter().collect();
        assert_eq!(
            got,
            HashSet::from(["http_request".to_string(), "browser".to_string()])
        );
    }

    #[test]
    fn clear_session_grants_empties_only_that_session() {
        let grants = SessionGrants::new();
        let a = "sess-grants-clear-a-7b2d";
        let b = "sess-grants-clear-b-7b2d";
        grants.record(a, &HashSet::from(["browser".to_string()]));
        grants.record(b, &HashSet::from(["shell".to_string()]));
        grants.clear(a);
        assert!(grants.granted_tools(a).is_empty());
        assert_eq!(grants.granted_tools(b), vec!["shell".to_string()]);
    }

    #[test]
    fn clear_all_empties_every_session() {
        let grants = SessionGrants::new();
        grants.record("sess-a", &HashSet::from(["browser".to_string()]));
        grants.record("sess-b", &HashSet::from(["shell".to_string()]));
        grants.clear_all();
        assert!(grants.granted_tools("sess-a").is_empty());
        assert!(grants.granted_tools("sess-b").is_empty());
    }

    #[test]
    fn new_session_past_the_cap_is_not_persisted_but_existing_ones_still_grow() {
        let grants = SessionGrants::new();
        for n in 0..MAX_GRANT_SESSIONS {
            grants.record(
                &format!("sess-{n}"),
                &HashSet::from(["browser".to_string()]),
            );
        }
        grants.record("sess-over-cap", &HashSet::from(["shell".to_string()]));
        assert!(grants.granted_tools("sess-over-cap").is_empty());
        grants.record("sess-0", &HashSet::from(["shell".to_string()]));
        assert_eq!(grants.granted_tools("sess-0").len(), 2);
    }

    /// The free functions are what the gateway calls. Another test may call
    /// `clear_all_session_grants` (through `apply_preset_to_config`) between the
    /// record and the read, so the round trip gets a few attempts. A function
    /// that stops using the shared instance never passes any of them.
    ///
    /// The `clear_all_session_grants` case lives in this test, not a second
    /// one, so the two cases cannot clear each other's session. It records its
    /// own session id and asserts only on that id, so it neither reads nor
    /// depends on another test's grants.
    #[test]
    fn public_functions_share_one_process_wide_instance() {
        let sid = "sess-public-fns-shared-instance-4e8f";
        let tools = HashSet::from(["http_request".to_string()]);
        let round_trips = (0..5).any(|_| {
            record_session_grants(sid, &tools);
            session_granted_tools(sid) == vec!["http_request".to_string()]
        });
        assert!(round_trips, "recorded grant never read back");

        record_session_grants(sid, &tools);
        clear_session_grants(sid);
        assert!(session_granted_tools(sid).is_empty());

        // `clear_all_session_grants` drops a recorded grant. Only count an
        // attempt once the grant was readable just before the call, so a clear
        // from another test cannot make an empty read look like this call's
        // work.
        let all_cleared = (0..5).any(|_| {
            record_session_grants(sid, &tools);
            if session_granted_tools(sid) != vec!["http_request".to_string()] {
                return false;
            }
            clear_all_session_grants();
            session_granted_tools(sid).is_empty()
        });
        assert!(
            all_cleared,
            "clear_all_session_grants left the grant behind"
        );
    }
}
