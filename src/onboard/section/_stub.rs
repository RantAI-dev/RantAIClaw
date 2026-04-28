//! Stub trait shared by Wave 2C / 2D / 2E section files.
//!
//! Wave 3 replaces this with the real `SetupSection` trait at
//! `src/onboard/section/mod.rs` (with `async_trait`, `CompensatingAction`,
//! `should_run`, etc.). Until then, leaves can implement this minimal
//! synchronous trait so each Wave-2 agent can land independently and
//! `cargo check --all-targets` stays clean.
//!
//! Source-of-truth design: `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Setup section trait" + §"Cross-wave coordination — _stub.rs" in the
//! companion plan.

use anyhow::Result;

use crate::config::Config;
use crate::profile::Profile;

/// Minimal, single-section run-context. Wave 3 will widen this with
/// `answers_index` and a `rollback_log: Vec<CompensatingAction>` once the
/// orchestrator is in place.
pub struct SetupContext<'a> {
    pub profile: &'a Profile,
    pub config: &'a mut Config,
    pub interactive: bool,
}

/// Minimal section trait. Wave 3 will replace this with an `async_trait`
/// version that adds `should_run(&Config) -> bool`. Until then, leaves
/// must remain synchronous so they don't depend on a reactor at the
/// wrong layer.
pub trait SetupSection {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn is_already_configured(&self, profile: &Profile, config: &Config) -> bool;
    fn run(&self, ctx: &mut SetupContext) -> Result<()>;
    fn headless_hint(&self) -> &'static str;
}
