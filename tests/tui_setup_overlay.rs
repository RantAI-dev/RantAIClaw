//! Integration tests for SetupOverlay state machine.

use rantaiclaw::onboard::provision::{ProvisionEvent, Severity};
use rantaiclaw::tui::SetupOverlayState;

#[test]
fn overlay_appends_message_events_to_log() {
    let mut s = SetupOverlayState::new("WhatsApp Web pairing");
    s.handle_event(ProvisionEvent::Message {
        severity: Severity::Info,
        text: "Connecting…".into(),
    });
    assert_eq!(s.log_lines().len(), 1);
    assert!(s.log_lines()[0].contains("Connecting"));
}

#[test]
fn overlay_prompt_event_sets_active_prompt() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Prompt {
        id: "phone".into(),
        label: "Phone number".into(),
        default: None,
        secret: false,
    });
    assert_eq!(
        s.active_prompt().map(|p| p.label.as_str()),
        Some("Phone number")
    );
}
