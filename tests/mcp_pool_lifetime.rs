//! The gateway owns MCP server processes, not each chat request.
//!
//! Before this, every console turn spawned every configured server, ran the
//! handshake and `tools/list`, then SIGKILLed the lot when the request's agent
//! dropped. These tests pin the two properties that fixes: a server is spawned
//! once across requests, and changing `mcp_servers` replaces the pool.

use std::collections::HashMap;

use rantaiclaw::config::schema::McpServerConfig;
use rantaiclaw::mcp::discover::McpPoolHandle;

/// A server that records one line per spawn, so the test can count processes
/// without watching the process table. POSIX shell only.
fn recording_server(marker: &std::path::Path) -> McpServerConfig {
    let script = format!(
        r#"
echo spawned >> '{marker}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *initialize*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"capabilities":{{}}}}}}\n' "$id" ;;
    *tools/list*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[{{"name":"ping","description":"","inputSchema":{{}}}}]}}}}\n' "$id" ;;
    *) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[]}}}}\n' "$id" ;;
  esac
done
"#,
        marker = marker.display()
    );
    McpServerConfig {
        command: "sh".into(),
        args: vec!["-c".into(), script],
        env: HashMap::new(),
    }
}

fn spawn_count(marker: &std::path::Path) -> usize {
    std::fs::read_to_string(marker)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

#[tokio::test]
async fn two_requests_share_one_server_process() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let marker = tmp.path().join("spawns");
    let mut servers = HashMap::new();
    servers.insert("fixture".to_string(), recording_server(&marker));

    let pool = McpPoolHandle::default();

    // Two turns, the way two chat requests reach it.
    let first = pool.current(&servers).await;
    let second = pool.current(&servers).await;

    assert_eq!(
        spawn_count(&marker),
        1,
        "a second request must reuse the running server, not respawn it"
    );
    assert_eq!(
        first.tools().len(),
        1,
        "the pooled server's tools are served"
    );
    assert_eq!(second.tools().len(), 1);
}

#[tokio::test]
async fn changing_the_configured_servers_rebuilds_the_pool() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let marker = tmp.path().join("spawns");

    let mut servers = HashMap::new();
    servers.insert("fixture".to_string(), recording_server(&marker));
    let pool = McpPoolHandle::default();
    let _first = pool.current(&servers).await;
    assert_eq!(spawn_count(&marker), 1);

    // The operator edits `[mcp_servers.fixture]`; the gateway hot-reloads
    // config, and the next turn must not keep talking to the old process.
    let mut changed = HashMap::new();
    let mut cfg = recording_server(&marker);
    cfg.env
        .insert("RANTAICLAW_FIXTURE".into(), "changed".into());
    changed.insert("fixture".to_string(), cfg);
    let _second = pool.current(&changed).await;

    assert_eq!(
        spawn_count(&marker),
        2,
        "a changed server config must produce a new process"
    );

    // And an unchanged config after that is still a no-op.
    let _third = pool.current(&changed).await;
    assert_eq!(spawn_count(&marker), 2);
}

#[tokio::test]
async fn an_empty_server_map_needs_no_processes() {
    let servers: HashMap<String, McpServerConfig> = HashMap::new();
    let pool = McpPoolHandle::default();
    let current = pool.current(&servers).await;
    assert!(current.tools().is_empty());
    assert!(current.health().is_empty());
}
