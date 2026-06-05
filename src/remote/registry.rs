//! Process-global SSH session registry, keyed `user@host:port`, shared by the
//! `ssh` and `pty` tools. Sessions are reference-counted so an in-flight exec
//! keeps the connection alive even if another caller disconnects it.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tokio::sync::Mutex;

use super::session::SshConn;

type Sessions = HashMap<String, Arc<SshConn>>;

fn sessions() -> &'static Mutex<Sessions> {
    static REG: OnceLock<Mutex<Sessions>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Store (or replace) a session under its id.
pub async fn insert(id: String, conn: Arc<SshConn>) {
    sessions().lock().await.insert(id, conn);
}

/// Fetch a live session by id, if present.
pub async fn get(id: &str) -> Option<Arc<SshConn>> {
    sessions().lock().await.get(id).cloned()
}

/// Drop a session from the registry. Returns true if one was present.
pub async fn remove(id: &str) -> bool {
    sessions().lock().await.remove(id).is_some()
}
