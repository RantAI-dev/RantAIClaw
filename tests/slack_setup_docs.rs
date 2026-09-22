//! Guard: docs §4.3 must list every Slack scope and event the setup paths
//! require. The wizard, the provisioner and the docs each used to list a
//! different subset of `chat:write`, `channels:history`, `im:history`,
//! `groups:history`, `mpim:history`, `files:read`, `files:write`, and the
//! Socket Mode / Event Subscriptions / App Home toggles. An operator following
//! any of them got a bot that could not see DMs or send files. This test
//! asserts the §4.3 prose covers the full set, and preserves the DM-filter
//! paragraph from #844 verbatim so the fix cannot regress the run-time
//! behaviour.

#[test]
fn slack_setup_docs_list_every_scope_socket_mode_needs() {
    let md = include_str!("../docs/reference/channels.md");

    // Slice off §4.3 only — the wizard test module is irrelevant here, but
    // matching the existing pattern (see whatsapp_web::production_half) keeps
    // the test self-contained and unambiguous about which section it covers.
    let section_start = md
        .find("### 4.3 Slack")
        .expect("docs §4.3 Slack must exist");
    let section_end = md[section_start..]
        .find("\n### 4.4")
        .expect("docs §4.4 must follow §4.3");
    let section = &md[section_start..section_start + section_end];

    for literal in [
        "chat:write",
        "channels:history",
        "im:history",
        "groups:history",
        "mpim:history",
        "files:read",
        "files:write",
        "Socket Mode",
        "message.im",
        "message.channels",
        "message.groups",
        "message.mpim",
        "App Home",
    ] {
        assert!(
            section.contains(literal),
            "docs §4.3 Slack must mention `{literal}`"
        );
    }

    // The DM-filter paragraph from #844 must remain verbatim — a Slack DM's
    // `D…` id can never equal a configured `C…` or `G…` id, and #844 changed
    // the runtime filter so DMs reach the bot. The prose names every
    // `channel_type` the filter looks at; deleting it lets the runtime and
    // the docs drift apart again.
    for fragment in [
        "`channel_type` `channel`",
        "`channel_type` `group`",
        "`channel_type` `mpim`",
        "`channel_type` `im`",
    ] {
        assert!(
            section.contains(fragment),
            "docs §4.3 must preserve the #844 DM-filter prose including {fragment}"
        );
    }
}
