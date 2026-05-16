//! Shared test helper — serializes env-mutating tests across all submodules
//! so concurrent test threads don't clobber `RANTAICLAW_HOME` /
//! `RANTAICLAW_KANBAN_*`.

use std::path::Path;

use parking_lot::Mutex;

pub static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Run `f` with `RANTAICLAW_HOME` pointed at a fresh tempdir and every other
/// `RANTAICLAW_KANBAN_*` env var cleared. The tempdir is restored to the
/// previous environment when `f` returns.
pub fn with_home<F: FnOnce()>(home: &Path, f: F) {
    let _guard = ENV_LOCK.lock();
    let prev_h = std::env::var("RANTAICLAW_HOME").ok();
    let prev_kh = std::env::var("RANTAICLAW_KANBAN_HOME").ok();
    let prev_kb = std::env::var("RANTAICLAW_KANBAN_BOARD").ok();
    let prev_kdb = std::env::var("RANTAICLAW_KANBAN_DB").ok();
    let prev_kt = std::env::var("RANTAICLAW_KANBAN_TASK").ok();
    let prev_orch = std::env::var("RANTAICLAW_KANBAN_ORCHESTRATOR").ok();
    std::env::remove_var("RANTAICLAW_KANBAN_HOME");
    std::env::remove_var("RANTAICLAW_KANBAN_BOARD");
    std::env::remove_var("RANTAICLAW_KANBAN_DB");
    std::env::remove_var("RANTAICLAW_KANBAN_TASK");
    std::env::remove_var("RANTAICLAW_KANBAN_ORCHESTRATOR");
    std::env::set_var("RANTAICLAW_HOME", home);
    f();
    match prev_h {
        Some(v) => std::env::set_var("RANTAICLAW_HOME", v),
        None => std::env::remove_var("RANTAICLAW_HOME"),
    }
    match prev_kh {
        Some(v) => std::env::set_var("RANTAICLAW_KANBAN_HOME", v),
        None => std::env::remove_var("RANTAICLAW_KANBAN_HOME"),
    }
    match prev_kb {
        Some(v) => std::env::set_var("RANTAICLAW_KANBAN_BOARD", v),
        None => std::env::remove_var("RANTAICLAW_KANBAN_BOARD"),
    }
    match prev_kdb {
        Some(v) => std::env::set_var("RANTAICLAW_KANBAN_DB", v),
        None => std::env::remove_var("RANTAICLAW_KANBAN_DB"),
    }
    match prev_kt {
        Some(v) => std::env::set_var("RANTAICLAW_KANBAN_TASK", v),
        None => std::env::remove_var("RANTAICLAW_KANBAN_TASK"),
    }
    match prev_orch {
        Some(v) => std::env::set_var("RANTAICLAW_KANBAN_ORCHESTRATOR", v),
        None => std::env::remove_var("RANTAICLAW_KANBAN_ORCHESTRATOR"),
    }
}

/// Run `f` with a fresh tempdir as `RANTAICLAW_HOME`.
pub fn with_temp_home<F: FnOnce(&Path)>(f: F) {
    let tmp = tempfile::tempdir().expect("kanban tempdir");
    let path = tmp.path().to_path_buf();
    with_home(&path, || f(&path));
}
