//! The lock the tests that touch the process-global proxy state take, beside
//! the environment lock in [`crate::test_env`] and the extraction-slot lock in
//! [`crate::test_slot`].
//!
//! A test that needs a process-global resource to itself takes a lock **in
//! the test code**, never through a CI flag. The suite runs in parallel, and
//! a flag that serializes the whole suite is a workaround for a lock that is
//! missing.

use tokio::sync::Mutex;

/// Serializes the tests that read or write the process-global proxy state.
///
/// **The shared resource:** `config::schema` keeps the runtime proxy
/// configuration in one process-global slot (`RUNTIME_PROXY_CONFIG`) and the
/// HTTP clients built from it in another (`RUNTIME_PROXY_CLIENT_CACHE`).
/// `set_runtime_proxy_config` writes the first and **empties** the second, so
/// a test that reads the cache and a test that sets the config cannot run at
/// the same time: the clear takes away the entry the reader just made, and
/// the assertion fails for a reason that has nothing to do with the code
/// under test. The same holds for the proxy itself — a test that leaves a
/// proxy in the global config hands it to every test that builds a client
/// afterwards.
///
/// The lock is in the test code on purpose: the global proxy configuration is
/// a production guarantee — one process, one proxy — and it does not change
/// here.
///
/// **The order:** a test that takes this lock takes the environment lock
/// ([`crate::test_env::ENV_LOCK`]) **first**. `apply_env_overrides` writes the
/// proxy variables into the process environment *and* empties the client
/// cache, so the two locks travel together — and the order is the same
/// everywhere, so no test can hold one while waiting for the other.
pub(crate) static PROXY_STATE_LOCK: Mutex<()> = Mutex::const_new(());
