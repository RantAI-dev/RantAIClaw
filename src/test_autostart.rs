//! The lock the tests that touch the process-global auto-start state take,
//! beside the environment lock in [`crate::test_env`], the extraction-slot
//! lock in [`crate::test_slot`] and the proxy-state lock in
//! [`crate::test_proxy`].
//!
//! A test that needs a process-global resource to itself takes a lock **in
//! the test code**, never through a CI flag. The suite runs in parallel, and
//! a flag that serializes the whole suite is a workaround for a lock that is
//! missing.

use tokio::sync::Mutex;

/// Serializes the tests that read or write the process-global auto-start
/// state.
///
/// **The shared resource:** `channels::auto_start_state` keeps the state of
/// the channel auto-start in one process-global cell (`STATE`). The TUI's
/// restart path writes it (`mark_starting`, `mark_terminated`,
/// `mark_failed`), and the `/channels` and `/doctor` panels read it back
/// (`snapshot`, `looks_running`). Two tests that touch it at the same time
/// therefore make one of them read a state the other is mid-transition into:
/// a panel built while a restart is in flight shows `Starting` — and, because
/// `Starting` carries the time it started at, a different runtime row —
/// instead of the state the test set up.
///
/// Every test that touches the auto-start state takes this lock first, so the
/// state is never contended inside a test binary.
///
/// It is a `tokio::sync::Mutex` (not `std::sync::Mutex`) so an async test can
/// hold the guard across `.await` points; `blocking_lock()` covers the sync
/// callers, which run outside any runtime.
///
/// **The order:** a test that takes this lock takes the environment lock
/// ([`crate::test_env::ENV_LOCK`]) and the proxy lock
/// ([`crate::test_proxy::PROXY_STATE_LOCK`]) **first**. The order is the same
/// everywhere, so no test can hold one while waiting for the other.
pub(crate) static AUTO_START_STATE_LOCK: Mutex<()> = Mutex::const_new(());
