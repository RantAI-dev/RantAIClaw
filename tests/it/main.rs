//! The package's single integration test binary.
//!
//! Every former `tests/<name>.rs` file is a module of this binary: one
//! `mod <name>;` per file. One binary means the whole suite links the library
//! and its dependency tree once, instead of one test binary per file.
//!
//! Two `snapshots` directories exist, and the difference matters:
//!
//! - `tests/it/snapshots/` holds the [insta] snapshots. insta derives a
//!   snapshot file's name from `module_path!()`, so a snapshot taken in
//!   `mod doctor_checks` is `it__doctor_checks__doctor_text.snap` — the
//!   `it__` prefix is this test crate's own module path.
//! - `tests/snapshots/` holds the six `persona_*.snap` files. They are not
//!   written by insta: a local `assert_snapshot` helper in
//!   `tests/it/persona_rendering.rs` reads and writes them through
//!   `CARGO_MANIFEST_DIR/tests/snapshots/…`, so they keep their old names and
//!   their old location.
//!
//! [insta]: https://docs.rs/insta

// The shared helper module, declared ONCE at the crate root. Every test file
// reaches it as `crate::common`, so the process-wide locks it owns are one
// static shared by the whole binary rather than one copy per file.
mod common;

mod agent_e2e;
mod agent_loop_robustness;
mod auth_command_removed;
mod channel_routing;
mod compat_v041_to_v050;
mod config_api;
mod config_doc_defaults;
mod config_migration_roundtrip;
mod config_persistence;
mod cron_api;
mod cross_root_marker;
mod dev_root_redirect;
mod dockerignore_test;
mod doctor_checks;
mod gateway_provider_auth;
mod kb;
mod mcp_client_transport;
mod mcp_pool_lifetime;
mod memory_recall_quality;
mod memory_restart;
mod migrate_legacy;
mod onboard_config_target;
mod onboard_mcp_section;
mod onboard_skills_section;
mod otel_dependency_feature_regression;
mod persona_rendering;
mod persona_timezone;
mod policy_writer;
mod profile_daemon_handoff;
mod profile_db_isolation;
mod profile_lifecycle;
mod profile_selection_doors;
mod provider_resolution;
mod provider_schema;
mod provision_whatsapp_web;
mod reply_target_field_regression;
mod schema_drift;
mod setup_e2e;
mod setup_orchestration;
mod slack_setup_docs;
mod sqlite_old_files_open;
mod tui_agent_bridge;
mod tui_integration;
mod tui_setup_overlay;
mod whatsapp_webhook_security;
