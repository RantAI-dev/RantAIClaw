//! Doctor — health checks across config, live API, and system deps.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Doctor command".

use async_trait::async_trait;

use crate::config::Config;
use crate::profile::Profile;

pub mod checks;
pub mod legacy;
pub mod report;

#[allow(unused_imports)]
pub use legacy::run;
pub use legacy::run_models;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Ok,
    Warn,
    Fail,
    Info,
}

impl Severity {
    pub fn icon(self) -> &'static str {
        match self {
            Severity::Ok => "✓",
            Severity::Warn => "⚠",
            Severity::Fail => "✗",
            Severity::Info => "ℹ",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Fail => "fail",
            Severity::Info => "info",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: String,
    pub severity: Severity,
    pub message: String,
    pub hint: Option<String>,
    pub duration_ms: u64,
    pub category: &'static str,
}

impl CheckResult {
    pub fn ok(name: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            severity: Severity::Ok,
            message: msg.into(),
            hint: None,
            duration_ms: 0,
            category: "config",
        }
    }
    pub fn warn(name: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            severity: Severity::Warn,
            message: msg.into(),
            hint: None,
            duration_ms: 0,
            category: "config",
        }
    }
    pub fn fail(name: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            severity: Severity::Fail,
            message: msg.into(),
            hint: None,
            duration_ms: 0,
            category: "config",
        }
    }
    pub fn info(name: impl Into<String>, msg: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            severity: Severity::Info,
            message: msg.into(),
            hint: None,
            duration_ms: 0,
            category: "config",
        }
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
    pub fn with_category(mut self, category: &'static str) -> Self {
        self.category = category;
        self
    }
    pub fn with_duration_ms(mut self, ms: u64) -> Self {
        self.duration_ms = ms;
        self
    }
}

pub struct DoctorContext {
    pub profile: Profile,
    pub config: Config,
    pub offline: bool,
}

#[async_trait]
pub trait DoctorCheck: Send + Sync {
    fn name(&self) -> &'static str;
    fn category(&self) -> &'static str;
    async fn run(&self, ctx: &DoctorContext) -> CheckResult;
}

async fn run_one(check: &dyn DoctorCheck, ctx: &DoctorContext) -> CheckResult {
    let started = std::time::Instant::now();
    let mut result = check.run(ctx).await;
    let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    if result.duration_ms == 0 {
        result.duration_ms = elapsed;
    }
    if result.category == "config" && check.category() != "config" {
        result.category = check.category();
    }
    result
}

pub async fn run_all(ctx: DoctorContext, brief: bool) -> Vec<CheckResult> {
    let registry: Vec<Box<dyn DoctorCheck>> = vec![
        Box::new(checks::config::ConfigSchemaCheck),
        Box::new(checks::config::PathsCheck),
        Box::new(checks::policy::AllowlistCheck),
        Box::new(checks::provider::ProviderPingCheck::default()),
        Box::new(checks::channels::ChannelsAuthCheck),
        Box::new(checks::mcp::McpStartupCheck),
        Box::new(checks::daemon::DaemonRegistrationCheck),
        Box::new(checks::system_deps::SystemDepsCheck),
    ];
    let mut results = Vec::with_capacity(registry.len());
    for check in &registry {
        if brief && check.category() == "live" {
            continue;
        }
        results.push(run_one(check.as_ref(), &ctx).await);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_icons_are_single_glyph() {
        assert_eq!(Severity::Ok.icon(), "✓");
        assert_eq!(Severity::Warn.icon(), "⚠");
        assert_eq!(Severity::Fail.icon(), "✗");
        assert_eq!(Severity::Info.icon(), "ℹ");
    }

    #[test]
    fn severity_strings_are_stable() {
        assert_eq!(Severity::Ok.as_str(), "ok");
        assert_eq!(Severity::Warn.as_str(), "warn");
        assert_eq!(Severity::Fail.as_str(), "fail");
        assert_eq!(Severity::Info.as_str(), "info");
    }

    #[test]
    fn check_result_builder_chains() {
        let r = CheckResult::warn("name", "msg")
            .with_hint("run X")
            .with_category("live")
            .with_duration_ms(42);
        assert_eq!(r.severity, Severity::Warn);
        assert_eq!(r.hint.as_deref(), Some("run X"));
        assert_eq!(r.category, "live");
        assert_eq!(r.duration_ms, 42);
    }
}
