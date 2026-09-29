//! Snapshot + behaviour tests for the persona renderer.
//!
//! `insta` isn't on the offline dev-dependency list, so these are
//! hand-rolled snapshot tests: each test renders one preset with a fixed
//! input, compares against a `.snap` file alongside `tests/snapshots/`,
//! and prints a copy-paste hint on mismatch. Wave 3 can swap to
//! `insta::assert_snapshot!` later — the on-disk shape is identical.
//!
//! Snapshots are deliberately committed; keep them in sync with template
//! tweaks via `UPDATE_SNAPSHOTS=1 cargo test --test persona_rendering`.

use std::fs;
use std::path::{Path, PathBuf};

use rantaiclaw::persona::{
    self, read_persona_toml, template_for, write_persona_toml, PersonaToml, PresetId,
};
use rantaiclaw::profile::ProfileManager;
use tempfile::TempDir;

fn snapshot_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join(format!("persona_{name}.snap"))
}

/// Compare `actual` against the snapshot file at `name`. On mismatch:
///   - if `UPDATE_SNAPSHOTS=1`, overwrite the snapshot with `actual`;
///   - else fail with a colourless diff hint.
///
/// Missing snapshot files are auto-created so first-run is friction-free.
fn assert_snapshot(name: &str, actual: &str) {
    let path = snapshot_path(name);
    let update = std::env::var("UPDATE_SNAPSHOTS").is_ok();
    if !path.exists() || update {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, actual).unwrap();
        if update {
            eprintln!("[persona_rendering] updated snapshot {}", path.display());
        }
        return;
    }
    let expected =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    if expected != actual {
        panic!(
            "snapshot mismatch for {}\n\
             --- expected ---\n{expected}\n\
             --- actual ---\n{actual}\n\
             rerun with UPDATE_SNAPSHOTS=1 to refresh",
            path.display()
        );
    }
}

// Fixed inputs so the snapshots are stable across runs.
const NAME: &str = "Shiro";
const TZ: &str = "Asia/Jakarta";
const ROLE: &str = "general productivity and helpful assistance";
const TONE: &str = "neutral";

// ── Per-preset snapshots, avoid=None ──────────────────────────────────

#[test]
fn snapshot_default_no_avoid() {
    let out =
        persona::renderer::render(template_for(PresetId::Default), NAME, TZ, ROLE, TONE, None);
    assert!(!out.contains("Things to avoid"));
    assert_snapshot("default_no_avoid", &out);
}

#[test]
fn snapshot_concise_pro_no_avoid() {
    let out = persona::renderer::render(
        template_for(PresetId::ConcisePro),
        NAME,
        TZ,
        ROLE,
        "formal",
        None,
    );
    assert!(!out.contains("Things to avoid"));
    assert_snapshot("concise_pro_no_avoid", &out);
}

#[test]
fn snapshot_friendly_companion_no_avoid() {
    let out = persona::renderer::render(
        template_for(PresetId::FriendlyCompanion),
        NAME,
        TZ,
        ROLE,
        "casual",
        None,
    );
    assert!(!out.contains("Things to avoid"));
    assert_snapshot("friendly_companion_no_avoid", &out);
}

#[test]
fn snapshot_research_analyst_no_avoid() {
    let out = persona::renderer::render(
        template_for(PresetId::ResearchAnalyst),
        NAME,
        TZ,
        "literature reviews and synthesis",
        TONE,
        None,
    );
    assert!(!out.contains("Things to avoid"));
    assert_snapshot("research_analyst_no_avoid", &out);
}

#[test]
fn snapshot_executive_assistant_no_avoid() {
    let out = persona::renderer::render(
        template_for(PresetId::ExecutiveAssistant),
        NAME,
        TZ,
        "calendar and inbox triage",
        TONE,
        None,
    );
    assert!(!out.contains("Things to avoid"));
    assert_snapshot("executive_assistant_no_avoid", &out);
}

// ── avoid-block guard behaviour ───────────────────────────────────────

#[test]
fn snapshot_executive_assistant_with_avoid() {
    let out = persona::renderer::render(
        template_for(PresetId::ExecutiveAssistant),
        NAME,
        TZ,
        "calendar and inbox triage",
        TONE,
        Some("medical advice or legal advice"),
    );
    assert!(out.contains("Things to avoid: medical advice or legal advice"));
    assert!(!out.contains("{{#if"));
    assert!(!out.contains("{{/if"));
    assert_snapshot("executive_assistant_with_avoid", &out);
}

#[test]
fn avoid_none_strips_block_in_every_preset() {
    for &p in PresetId::ALL {
        let out = persona::renderer::render(template_for(p), NAME, TZ, ROLE, TONE, None);
        assert!(
            !out.contains("Things to avoid"),
            "preset {p:?} leaked the avoid block when avoid=None"
        );
        assert!(!out.contains("{{#if"), "preset {p:?} leaked block opener");
        assert!(!out.contains("{{/if"), "preset {p:?} leaked block closer");
    }
}

#[test]
fn avoid_some_keeps_block_in_every_preset() {
    for &p in PresetId::ALL {
        let out = persona::renderer::render(
            template_for(p),
            NAME,
            TZ,
            ROLE,
            TONE,
            Some("internal-only topics"),
        );
        assert!(
            out.contains("Things to avoid: internal-only topics"),
            "preset {p:?} dropped the avoid block when avoid=Some"
        );
        assert!(!out.contains("{{#if"), "preset {p:?} leaked block opener");
        assert!(!out.contains("{{/if"), "preset {p:?} leaked block closer");
    }
}

#[test]
fn avoid_whitespace_only_treated_as_none() {
    let out = persona::renderer::render(
        template_for(PresetId::Default),
        NAME,
        TZ,
        ROLE,
        TONE,
        Some("   \n  "),
    );
    assert!(!out.contains("Things to avoid"));
}

// ── Round-trip + on-disk wiring ───────────────────────────────────────

#[test]
fn write_persona_toml_round_trips() {
    let tmp = TempDir::new().unwrap();
    std::env::set_var("HOME", tmp.path());
    // Avoid leaking a sticky env between tests.
    std::env::remove_var("RANTAICLAW_PROFILE");
    let profile = ProfileManager::ensure("rt-roundtrip").unwrap();

    let persona = PersonaToml {
        preset: PresetId::ConcisePro,
        name: "Shiro".into(),
        timezone: "Asia/Jakarta".into(),
        role: "engineering work".into(),
        tone: "formal".into(),
        avoid: Some("speculative claims".into()),
        always_on_kbs: Vec::new(),
    };
    write_persona_toml(&profile, &persona).unwrap();

    let back = read_persona_toml(&profile).unwrap().expect("file exists");
    assert_eq!(persona, back);
}

#[test]
fn render_produces_a_substituted_body_with_no_leftover_placeholders() {
    // The persona renders straight to the `## Persona` prompt section (no
    // on-disk SYSTEM.md file any more), so assert the rendered body directly.
    let persona = PersonaToml::default_for("Shiro", "Asia/Jakarta");
    let body = persona.render();
    assert!(body.contains("Shiro"));
    assert!(body.contains("Asia/Jakarta"));
    assert!(!body.contains("{{name}}"));
    assert!(!body.contains("{{#if"));
}

// `is_already_configured` is tested in the section's `#[cfg(test)]` block
// since the `onboard` module is `pub(crate)` and not reachable from this
// integration test.

#[test]
fn renders_without_a_blank_name_gap() {
    // A legacy persona with an empty name must not render "assistant for  (…".
    let persona = PersonaToml::default_for("", "UTC");
    let body = persona.render();
    assert!(
        !body.contains("for  ("),
        "empty name must not leave a double-space gap"
    );
    assert!(!body.contains("{{"), "no leftover placeholders");
}

#[test]
fn guest_render_omits_owner_name_and_timezone_in_every_preset() {
    for &p in PresetId::ALL {
        let persona = PersonaToml {
            preset: p,
            name: "Owner Name".into(),
            timezone: "Asia/Jakarta".into(),
            role: ROLE.into(),
            tone: TONE.into(),
            avoid: None,
            always_on_kbs: Vec::new(),
        };

        let owner_body = persona.render();
        assert!(
            owner_body.contains("Owner Name"),
            "preset {p:?} owner render should contain the owner's name"
        );
        assert!(
            owner_body.contains("Asia/Jakarta"),
            "preset {p:?} owner render should contain the owner's timezone"
        );

        let guest_body = persona.render_for_guest();
        assert!(
            !guest_body.contains("Owner Name"),
            "preset {p:?} guest render leaked the owner's name: {guest_body}"
        );
        assert!(
            !guest_body.contains("Asia/Jakarta"),
            "preset {p:?} guest render leaked the owner's timezone: {guest_body}"
        );
        assert!(
            !guest_body.contains("{{"),
            "preset {p:?} leaked a placeholder"
        );
    }
}

/// A guest reads the persona as the bot's brief for the person in the chat, so
/// a line such as "mirror <name>'s energy" must name that person as "the user".
/// Substituting a description of the owner ("the owner of this bot") tells a
/// guest that the person they are talking to is somebody else.
#[test]
fn guest_render_addresses_the_person_in_the_chat_in_every_preset() {
    for &p in PresetId::ALL {
        let persona = PersonaToml {
            preset: p,
            name: "Owner Name".into(),
            timezone: "Asia/Jakarta".into(),
            role: ROLE.into(),
            tone: TONE.into(),
            avoid: None,
            always_on_kbs: Vec::new(),
        };

        let guest_body = persona.render_for_guest();
        assert!(
            !guest_body.contains("the owner of this bot"),
            "preset {p:?} guest render still describes the person in the chat as the owner: {guest_body}"
        );
        assert!(
            guest_body.contains("for the user"),
            "preset {p:?} guest render should address the person in the chat as the user: {guest_body}"
        );
    }
}

/// The executive-assistant preset's scheduling bullet mentions `{{timezone}}`
/// outside the line-3 `{{#if timezone}}` guard. An empty timezone (every
/// guest render, and any owner who has not set one) used to substitute the
/// placeholder with an empty string, leaving "relative to ." rather than
/// dropping the whole timezone-dependent sentence.
#[test]
fn empty_timezone_drops_the_scheduling_bullet_in_executive_assistant() {
    let out = persona::renderer::render(
        template_for(PresetId::ExecutiveAssistant),
        NAME,
        "",
        "calendar and inbox triage",
        TONE,
        None,
    );
    assert!(
        !out.contains("relative to ."),
        "must not leave a dangling timezone phrase: {out}"
    );
    assert!(
        !out.contains("scheduling-adjacent"),
        "the whole timezone-dependent bullet must be dropped without a timezone: {out}"
    );
    assert!(!out.contains("{{"), "no leftover placeholders: {out}");
}

/// Control: a non-empty timezone keeps the bullet, worded exactly as before —
/// this is what the `executive_assistant_no_avoid` snapshot (rendered with a
/// timezone) already locks in byte-for-byte.
#[test]
fn nonempty_timezone_keeps_the_scheduling_bullet_in_executive_assistant() {
    let out = persona::renderer::render(
        template_for(PresetId::ExecutiveAssistant),
        NAME,
        TZ,
        "calendar and inbox triage",
        TONE,
        None,
    );
    assert!(
        out.contains("relative to Asia/Jakarta"),
        "a configured timezone must keep the scheduling bullet: {out}"
    );
}

#[test]
fn placeholder_in_a_field_value_is_not_re_expanded() {
    // A value that itself contains a `{{role}}` token must survive verbatim —
    // sequential replaces used to expand it with the later role substitution.
    let persona = PersonaToml {
        preset: PresetId::Default,
        name: "{{role}}".to_string(),
        timezone: "UTC".to_string(),
        role: "analyst".to_string(),
        tone: "neutral".to_string(),
        avoid: None,
        always_on_kbs: Vec::new(),
    };
    let body = persona.render();
    assert!(
        body.contains("{{role}}"),
        "the literal name token must not be re-expanded: {body}"
    );
}
