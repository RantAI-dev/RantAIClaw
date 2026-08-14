//! The SetupOverlay state machine, driven by real `ProvisionEvent`s.
//!
//! Two tests were removed rather than kept: they built a
//! `CommandResult::OpenSetupOverlay { .. }` and asserted the field they had
//! just written. They exercised no production code and could not fail.
//! What remains drives `SetupOverlayState::handle_event`, which is production
//! code and is what the overlay's behaviour actually depends on.

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

#[test]
fn choose_event_sets_active_choose_state() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Choose {
        id: "tier".into(),
        label: "Pick a tier".into(),
        options: vec![
            "Manual".into(),
            "Smart".into(),
            "Strict".into(),
            "Off".into(),
        ],
        multi: false,
    });
    assert!(s.active_choose().is_some());
    let c = s.active_choose().unwrap();
    assert_eq!(c.label, "Pick a tier");
    assert_eq!(c.options.len(), 4);
    assert_eq!(c.cursor, 0);
    assert!(!c.multi);
    assert!(c.selected.is_empty());
}

#[test]
fn choose_single_select_submits_one_index() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Choose {
        id: "tier".into(),
        label: "x".into(),
        options: vec!["a".into(), "b".into(), "c".into()],
        multi: false,
    });
    s.choose_move_down();
    s.choose_move_down();
    let (id, sel) = s.submit_choose().expect("submit returns Some");
    assert_eq!(id, "tier");
    assert_eq!(sel, vec![2]);
    assert!(
        s.active_choose().is_none(),
        "submit must clear active choose"
    );
}

#[test]
fn choose_multi_select_toggles_with_space() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Choose {
        id: "skills".into(),
        label: "x".into(),
        options: vec!["a".into(), "b".into(), "c".into()],
        multi: true,
    });
    s.choose_toggle();
    s.choose_move_down();
    s.choose_move_down();
    s.choose_toggle();
    let (_, sel) = s.submit_choose().unwrap();
    assert_eq!(sel, vec![0, 2]);
}

#[test]
fn choose_single_select_ignores_toggle() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Choose {
        id: "x".into(),
        label: "x".into(),
        options: vec!["a".into(), "b".into()],
        multi: false,
    });
    s.choose_toggle();
    let (_, sel) = s.submit_choose().unwrap();
    assert_eq!(sel, vec![0]);
}

#[test]
fn choose_move_up_at_zero_stays_at_zero() {
    let mut s = SetupOverlayState::new("x");
    s.handle_event(ProvisionEvent::Choose {
        id: "x".into(),
        label: "x".into(),
        options: vec!["a".into(), "b".into()],
        multi: false,
    });
    s.choose_move_up();
    assert_eq!(s.active_choose().unwrap().cursor, 0);
}
