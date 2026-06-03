//! AXI ambient context for the agent loop (principle #7: self-install
//! into session hooks).
//!
//! When the KB feature is compiled in AND a KB database file exists on
//! disk, the agent's system prompt should learn that `rantaiclaw kb …`
//! is a callable shell capability. The agent then shells out via its
//! existing `shell` tool — there is intentionally **no** `Tool` trait
//! implementation for the KB.
//!
//! The detection logic is deliberately cheap: just `Path::exists()`
//! against the same path `KbCommand::run` would resolve. No SQLite
//! handshake, no schema probe — those would couple system-prompt
//! assembly to KB internals and slow every turn.

use std::path::Path;

use super::cli::resolve_kb_db_path;

/// AXI ambient-context one-liner for the agent's system prompt.
///
/// Returns `Some(text)` when a KB database is reachable at the resolved
/// path (`KB_DB_PATH` env → XDG data dir → `./kb.db`). Returns `None`
/// when no DB file is present — in that case the agent never learns
/// about the capability and never shells out to it, which is the
/// correct deny-by-default behavior.
pub fn kb_ambient_context() -> Option<String> {
    let path = resolve_kb_db_path();
    if !Path::new(&path).exists() {
        return None;
    }
    Some(
        "Knowledge base available. To search documents, run:\n\
         `rantaiclaw kb search \"<question>\" --top 5`\n\
         Output is TOON. List: `rantaiclaw kb list`. Detail: `rantaiclaw kb get <id>`."
            .to_string(),
    )
}
