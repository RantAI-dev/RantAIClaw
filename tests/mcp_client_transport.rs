//! Transport-level regression tests for the stdio MCP client.
//!
//! Both cases here are about a client that has to survive being *long-lived*.
//! While an `McpClient` lasted one chat request, neither shape had time to
//! appear; pooling servers across requests makes both routine.
//!
//! The fixtures are POSIX shell — no Python, no jq, nothing beyond what the
//! test box already runs `sh` with — so they stay honest about what is
//! available in CI.

use std::collections::HashMap;

use rantaiclaw::mcp::client::McpClient;

/// Reply to `initialize`, swallow `notifications/initialized`, then answer
/// every request as it arrives. `$PREFIX` runs once before the loop, which is
/// where a test injects whatever it wants the server to do first.
fn well_behaved_server(prefix: &str) -> String {
    format!(
        r#"
{prefix}
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *initialize*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"capabilities":{{}}}}}}\n' "$id" ;;
    *tools/list*) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"tools":[]}}}}\n' "$id" ;;
    *) printf '{{"jsonrpc":"2.0","id":%s,"result":{{"content":[{{"type":"text","text":"ok"}}]}}}}\n' "$id" ;;
  esac
done
"#
    )
}

async fn connect(script: &str) -> McpClient {
    McpClient::connect(
        "fixture",
        "sh",
        &["-c".to_string(), script.to_string()],
        &HashMap::new(),
    )
    .await
    .expect("fixture server connects")
}

/// stderr was piped and never read. A server that logs more than the pipe
/// buffer holds (~64 KiB on Linux) blocks forever on its own `write`, and the
/// client waits out the full request timeout against a process that is alive,
/// healthy, and stuck. Chatty servers are normal — npx prints install progress.
#[tokio::test]
async fn a_server_that_floods_stderr_still_answers() {
    // 256 KiB of stderr before the first reply — comfortably past any pipe
    // buffer, written with `yes`/`head` so no extra tooling is required.
    let script = well_behaved_server("yes rantaiclaw-log-line | head -c 262144 >&2");
    let client = connect(&script).await;

    let out = client
        .call("noop", serde_json::json!({}))
        .await
        .expect("a server that logged a lot must still be able to answer");
    assert_eq!(out, "ok");
}

/// Two calls in flight against one server. The reader used to be whoever asked
/// first: it read until it saw *its* id and dropped everything else, so the
/// other caller's reply went in the bin and that caller waited out the timeout.
///
/// The fixture answers in reverse arrival order, which is what makes the defect
/// deterministic rather than a race: whichever request is read first is
/// answered last.
#[tokio::test]
async fn two_concurrent_calls_both_get_their_own_reply() {
    let script = r#"
IFS= read -r init
init_id=$(printf '%s' "$init" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"capabilities":{}}}\n' "$init_id"
IFS= read -r _notification
IFS= read -r first
IFS= read -r second
first_id=$(printf '%s' "$first" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
second_id=$(printf '%s' "$second" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"second"}]}}\n' "$second_id"
printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"first"}]}}\n' "$first_id"
sleep 5
"#;
    let client = connect(script).await;

    let (a, b) = tokio::join!(
        client.call("alpha", serde_json::json!({})),
        client.call("beta", serde_json::json!({})),
    );

    let a = a.expect("the first caller must get a reply");
    let b = b.expect("the second caller must get a reply");
    // One of them was answered first and the other second; which is not the
    // point. The point is that neither reply was thrown away.
    let mut got = [a.as_str(), b.as_str()];
    got.sort_unstable();
    assert_eq!(got, ["first", "second"]);
}

/// A server that dies mid-request should fail its caller promptly, not leave it
/// waiting out the 30s request timeout on a pipe nobody will ever write to.
#[tokio::test]
async fn a_server_that_exits_fails_its_caller_without_waiting_out_the_timeout() {
    let script = r#"
IFS= read -r init
init_id=$(printf '%s' "$init" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"capabilities":{}}}\n' "$init_id"
IFS= read -r _notification
exit 0
"#;
    let client = connect(script).await;

    let started = std::time::Instant::now();
    let err = client
        .call("noop", serde_json::json!({}))
        .await
        .expect_err("a dead server cannot answer");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "should fail on EOF, not on the request timeout: {err:#}"
    );
}
