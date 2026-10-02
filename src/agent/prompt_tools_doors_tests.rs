//! A prompt names a tool only if the caller's registry holds it, and it words
//! the tool the way the tool does (`Tool::description`). Each door that builds
//! a tool list has its own case here or beside its code, so a door that stops
//! reading its registry fails by name.

use super::door_test_support::{assert_prompt_lists, held_by, registry_for, DoorFixture};

/// `agent -m`, `chat -m`, a cron agent job and the daemon heartbeat all end in
/// `agent::run_with_scope`. The prompt that door sends lists the tools its own
/// registry holds, each as the tool describes itself, and no other.
#[tokio::test]
async fn the_cli_door_lists_the_registry_tools_with_their_own_descriptions() {
    let fixture = DoorFixture::start().await;

    Box::pin(crate::agent::run(
        fixture.config.clone(),
        Some("hello".to_string()),
        None,
        None,
        0.0,
        "cli",
        true,
    ))
    .await
    .expect("the CLI turn runs against the local server");

    let registry = registry_for(&fixture.config);
    assert_prompt_lists("cli", &fixture.llm.last_system_text(), &held_by(&registry));
}
