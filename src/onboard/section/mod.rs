//! Setup-section root.
//!
//! Wave 3 lifts the `SetupSection` trait + `SetupContext` out of the Wave-2
//! `_stub.rs` placeholder and into this canonical home; Wave 4A then adds
//! the `approvals` section so the canonical order is now six section impls
//! (`provider`, `approvals`, `channels`, `persona`, `skills`, `mcp`)
//! for the orchestrator (`crate::onboard::wizard::run_setup`) to walk.
//!
//! The trait is intentionally synchronous: section bodies that need an
//! async runtime (skills' ClawHub fetcher, mcp's curated picker) bridge
//! over to `tokio` internally — keeping the trait sync avoids leaking
//! reactor assumptions into the orchestrator and matches Wave 2's design.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"5. Section catalog" + §"Wizard orchestrator".

use anyhow::Result;

use crate::config::Config;
use crate::profile::Profile;

pub mod approvals;
pub mod channels;
pub mod mcp;
pub mod persona;
pub mod provider;
pub mod skills;

/// Per-run context handed to every `SetupSection::run` call.
///
/// Sections read/write `config` directly; the orchestrator persists once
/// at the end of `run_setup` (no per-section staging file in v0.5.0 — the
/// two-phase commit described in the spec is Wave 4+ scope).
pub struct SetupContext<'a> {
    pub profile: &'a Profile,
    pub config: &'a mut Config,
    /// `true` only when stdin + stdout are TTYs and the user invoked
    /// `setup` without `--non-interactive`. Headless callers should bail
    /// with `headless_hint()` rather than prompting.
    pub interactive: bool,
}

/// One unit of onboarding. Wave 3 wires five concrete impls; later waves
/// add `workspace`, `tunnel`, `tools`, `hardware`, `memory`,
/// `project_context`, `workspace_files`, and `daemon`.
pub trait SetupSection {
    /// Stable, kebab-case identifier — also the value of
    /// `rantaiclaw setup <topic>`.
    fn name(&self) -> &'static str;

    /// One-line human description shown in the section header.
    fn description(&self) -> &'static str;

    /// Pure check — does the user already have this section configured?
    /// The orchestrator uses this to skip already-configured sections
    /// unless `--force` is set.
    fn is_already_configured(&self, profile: &Profile, config: &Config) -> bool;

    /// Execute the section. May prompt via `dialoguer` iff `ctx.interactive`.
    fn run(&self, ctx: &mut SetupContext) -> Result<()>;

    /// One-line CLI hint shown when the section bails in headless mode.
    fn headless_hint(&self) -> &'static str;
}
