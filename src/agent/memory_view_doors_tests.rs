//! The CLI door sets the `All` memory view. Every other door has its own case
//! beside the code it drives (channels, cron, daemon, gateway, tui, delegate).

use super::door_test_support::DoorFixture;

/// `agent -m` and `chat -m` both end in `agent::run`: the operator's own
/// terminal reads all of memory, so every note reaches the model, and the
/// prompt carries `USER.md` and `MEMORY.md`.
#[tokio::test]
async fn the_cli_door_reads_all_of_memory() {
    let fixture = DoorFixture::start().await;

    // The CLI single-shot path always passes `Some(message)`, so the REPL
    // branch never reads from this; any `AsyncBufRead + Unpin + Send`
    // satisfies the parameter.
    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let reply = Box::pin(crate::agent::run(
        fixture.config.clone(),
        Some("what about the lantern".to_string()),
        None,
        None,
        0.0,
        "cli",
        true,
        &mut stdin,
    ))
    .await
    .expect("the CLI turn runs against the local server");

    assert_eq!(reply, "Done.");
    fixture.assert_every_note_was_sent();
}
