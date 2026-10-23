//! Setup-section root.
//!
//! Wave 3 lifts the `SetupSection` trait + `SetupContext` out of the Wave-2
//! `_stub.rs` placeholder and into this canonical home; Wave 4A then adds
//! the `approvals` section, later waves add `login` and — with the `kb`
//! feature — `knowledge`. The canonical order is now seven section impls
//! (`provider`, `approvals`, `channels`, `persona`, `skills`, `mcp`,
//! `login`), plus `knowledge` in `kb` builds, for the orchestrator
//! (`crate::onboard::wizard::run_setup`) to walk.
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
#[cfg(feature = "kb")]
pub mod knowledge;
pub mod login;
pub mod mcp;
pub mod persona;
pub mod provider;
pub mod skills;

/// A section that needed a prompt this run could not give.
///
/// A headless run cannot prompt, so a section that needs one configures
/// nothing. It records itself here instead of only printing a hint, so the
/// orchestrator can name every such section at the end of the run — and the
/// command can exit non-zero instead of reporting a success it did not earn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedSection {
    /// The section's stable name (`provider`, `channels`, …).
    pub name: String,
    /// The command that does the work instead — the section's
    /// `headless_hint()`.
    pub hint: String,
}

/// Per-run context handed to every `SetupSection::run` call.
///
/// Sections read/write `config` directly; the orchestrator persists once
/// at the end of `run_setup` (no per-section staging file in v0.5.0 — the
/// two-phase commit described in the spec is Wave 4+ scope).
pub struct SetupContext<'a> {
    pub profile: &'a Profile,
    pub config: &'a mut Config,
    /// `true` only when stdin + stdout are TTYs and the user invoked
    /// `setup` without `--non-interactive`. A headless caller records the
    /// section as blocked — with its `headless_hint()` — instead of
    /// prompting; the orchestrator prints the hint once, in the run's
    /// summary.
    pub interactive: bool,
    /// `true` when the operator passed `--force` (re-run an already-configured
    /// section). Sections that write idempotent files (e.g. approvals policy)
    /// must honor this so `--force` actually overwrites instead of no-oping.
    pub force: bool,
    /// Sections that needed a prompt this run could not have. A headless
    /// section records itself here; the orchestrator drains the list into
    /// the run's report.
    pub blocked: Vec<BlockedSection>,
}

impl SetupContext<'_> {
    /// Record that this section could not run because it needed a prompt.
    ///
    /// The hint is what the operator runs instead. It is printed once, by
    /// the run's reporting path — not by the section as it gives up.
    pub fn record_blocked(&mut self, name: &str, hint: &str) {
        self.blocked.push(BlockedSection {
            name: name.to_string(),
            hint: hint.to_string(),
        });
    }
}

/// One unit of onboarding. Wave 3 wires the concrete impls; later waves
/// add `approvals`, `login`, and — with the `kb` feature — `knowledge`, so
/// the canonical list is now seven impls, plus `knowledge` in `kb` builds.
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
