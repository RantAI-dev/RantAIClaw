//! The process-global locks the library's tests take, beside the environment
//! lock in [`crate::test_env`].
//!
//! A test that needs the process to itself takes a lock **in the test code**,
//! never through a CI flag. The suite runs in parallel, and a flag that
//! serializes the whole suite is a workaround for a lock that is missing.
//!
//! One lock lives here today: the one the tests that reach the extraction
//! slot take.

use tokio::sync::Mutex;

/// Serializes the tests that reach the extraction slot.
///
/// **The shared resource:** `kb::extract::inbound` takes a **process-global**,
/// one-permit semaphore before every extraction, and the caller's own
/// deadline covers the wait for it. Two tests that reach the slot at the same
/// time therefore make one of them spend its deadline queueing, and the
/// extraction it then runs is refused for the wrong reason: "timed out after
/// N seconds waiting for the extraction slot".
///
/// Every test that reaches the slot takes this lock first, so the slot is
/// never contended inside a test binary and a test's deadline is spent on its
/// own document. The lock is in the test code on purpose: the one-at-a-time
/// slot is a production guarantee — a hostile parser must not stall the
/// agent — and it does not change here.
pub(crate) static EXTRACTION_SLOT_LOCK: Mutex<()> = Mutex::const_new(());
