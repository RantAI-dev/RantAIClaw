//! Config-schema and filesystem-path checks.

use std::path::Path;

use async_trait::async_trait;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext};

pub struct ConfigSchemaCheck;

#[async_trait]
impl DoctorCheck for ConfigSchemaCheck {
    fn name(&self) -> &'static str { "config.schema" }
    fn category(&self) -> &'static str { "config" }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let mut problems = Vec::new();
        let cfg = &ctx.config;

        match &cfg.default_provider {
            None => problems.push("no default_provider configured".to_string()),
            Some(name) => {
                if let Some(reason) = provider_validation_error(name) {
                    problems.push(format!("default_provider \"{name}\" is invalid: {reason}"));
                }
            }
        }

        if !(0.0..=2.0).contains(&cfg.default_temperature) {
            problems.push(format!(
                "temperature {:.2} out of range (expected 0.0–2.0)",
                cfg.default_temperature
            ));
        }

        if cfg.gateway.port == 0 {
            problems.push("gateway.port is 0 (invalid)".to_string());
        }

        for fb in &cfg.reliability.fallback_providers {
            if let Some(reason) = provider_validation_error(fb) {
                problems.push(format!("fallback provider \"{fb}\" invalid: {reason}"));
            }
        }

        if problems.is_empty() {
            CheckResult::ok(self.name(), "config schema is valid").with_category(self.category())
        } else {
            let summary = format!("{} problem(s)", problems.len());
            let detail = problems.join("; ");
            CheckResult::fail(self.name(), format!("{summary}: {detail}"))
                .with_category(self.category())
                .with_hint("run: rantaiclaw setup provider")
        }
    }
}

pub struct PathsCheck;

#[async_trait]
impl DoctorCheck for PathsCheck {
    fn name(&self) -> &'static str { "config.paths" }
    fn category(&self) -> &'static str { "config" }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let ws = &ctx.config.workspace_dir;
        if !ws.exists() {
            return CheckResult::fail(self.name(), format!("workspace_dir missing: {}", ws.display()))
                .with_category(self.category())
                .with_hint("run: rantaiclaw onboard --interactive");
        }
        match writable_probe(ws) {
            Ok(()) => CheckResult::ok(self.name(), format!("workspace at {} is writable", ws.display()))
                .with_category(self.category()),
            Err(e) => CheckResult::fail(self.name(), format!("workspace_dir not writable: {e}"))
                .with_category(self.category())
                .with_hint("check directory permissions"),
        }
    }
}

fn writable_probe(dir: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let probe = dir.join(format!(".doctor_probe_{}_{}", std::process::id(), nanos));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&probe)?;
    f.write_all(b"probe")?;
    drop(f);
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn provider_validation_error(name: &str) -> Option<String> {
    match crate::providers::create_provider(name, None) {
        Ok(_) => None,
        Err(err) => Some(err.to_string().lines().next().unwrap_or("invalid provider").to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::doctor::Severity;
    use crate::profile::Profile;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn ctx_with_config(cfg: Config) -> (DoctorContext, TempDir) {
        let tmp = TempDir::new().unwrap();
        let profile = Profile { name: "test".into(), root: tmp.path().to_path_buf() };
        (DoctorContext { profile, config: cfg, offline: false }, tmp)
    }

    #[tokio::test]
    async fn config_schema_check_passes_on_default_config() {
        let cfg = Config::default();
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ConfigSchemaCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
    }

    #[tokio::test]
    async fn config_schema_check_fails_on_bad_temperature() {
        let mut cfg = Config::default();
        cfg.default_temperature = 9.9;
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ConfigSchemaCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail);
        assert!(result.message.contains("temperature"));
    }

    #[tokio::test]
    async fn config_schema_check_fails_on_unknown_provider() {
        let mut cfg = Config::default();
        cfg.default_provider = Some("totally-fake".into());
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ConfigSchemaCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail);
        assert!(result.hint.is_some());
    }

    #[tokio::test]
    async fn paths_check_fails_when_workspace_missing() {
        let mut cfg = Config::default();
        cfg.workspace_dir = PathBuf::from("/nonexistent/rantaiclaw_doctor_test_path");
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = PathsCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail);
        assert!(result.hint.is_some());
    }

    #[tokio::test]
    async fn paths_check_passes_when_workspace_writable() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = Config::default();
        cfg.workspace_dir = tmp.path().to_path_buf();
        let (ctx, _hold) = ctx_with_config(cfg);
        let result = PathsCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok);
    }
}
