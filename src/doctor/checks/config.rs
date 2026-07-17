//! Config-schema and filesystem-path checks.

use std::path::Path;

use async_trait::async_trait;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext};

pub struct ConfigSchemaCheck;

#[async_trait]
impl DoctorCheck for ConfigSchemaCheck {
    fn name(&self) -> &'static str {
        "config.schema"
    }
    fn category(&self) -> &'static str {
        "config"
    }
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

/// Does the active provider have a key we could actually send a message with?
///
/// Deliberately separate from [`ConfigSchemaCheck`], which answers "is the
/// config well-formed" — a valid schema and a usable provider are different
/// questions, and `config.schema` was reporting "config schema is valid" on a
/// config that cannot talk to any model.
///
/// Category is `config`, not `live`: `run_all` skips every `live` check in
/// brief/offline mode, and a missing key is exactly what an offline check can
/// and should catch. It needs no network.
///
/// Resolution goes through [`Config::resolve_key_for_provider`] — the same
/// function the four real send paths use (`agent::agent`, `agent::loop_` ×2,
/// `gateway`). Every diagnostic previously re-implemented a weaker presence
/// test against the top-level `api_key` alone, so a key stored under
/// `provider_api_keys` (what the web console writes) read as absent.
pub struct ProviderKeyCheck;

#[async_trait]
impl DoctorCheck for ProviderKeyCheck {
    fn name(&self) -> &'static str {
        "config.provider_key"
    }
    fn category(&self) -> &'static str {
        "config"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let Some(provider) = ctx.config.default_provider.as_deref() else {
            // ConfigSchemaCheck already reports this; don't double-fail.
            return CheckResult::ok(self.name(), "no default_provider to check")
                .with_category(self.category());
        };

        if crate::providers::provider_is_local(provider) {
            return CheckResult::ok(
                self.name(),
                format!("{provider} runs locally — no API key needed"),
            )
            .with_category(self.category());
        }

        match ctx.config.resolve_key_for_provider(provider) {
            Some(_) => CheckResult::ok(self.name(), format!("API key resolved for {provider}"))
                .with_category(self.category()),
            None => CheckResult::fail(
                self.name(),
                format!("no API key for {provider} — the agent cannot send a message"),
            )
            .with_category(self.category())
            .with_hint("run: rantaiclaw setup provider"),
        }
    }
}

pub struct PathsCheck;

#[async_trait]
impl DoctorCheck for PathsCheck {
    fn name(&self) -> &'static str {
        "config.paths"
    }
    fn category(&self) -> &'static str {
        "config"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let ws = &ctx.config.workspace_dir;
        if !ws.exists() {
            return CheckResult::fail(
                self.name(),
                format!("workspace_dir missing: {}", ws.display()),
            )
            .with_category(self.category())
            .with_hint("run: rantaiclaw onboard --interactive");
        }
        match writable_probe(ws) {
            Ok(()) => CheckResult::ok(
                self.name(),
                format!("workspace at {} is writable", ws.display()),
            )
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
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    f.write_all(b"probe")?;
    drop(f);
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn provider_validation_error(name: &str) -> Option<String> {
    match crate::providers::create_provider(name, None) {
        Ok(_) => None,
        Err(err) => Some(
            err.to_string()
                .lines()
                .next()
                .unwrap_or("invalid provider")
                .to_string(),
        ),
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
        let profile = Profile {
            name: "test".into(),
            root: tmp.path().to_path_buf(),
        };
        (
            DoctorContext {
                profile,
                config: cfg,
                offline: false,
            },
            tmp,
        )
    }

    /// `Config::default()` IS schema-valid — provider name known, temperature
    /// in range, port non-zero. That is all this check claims, and it is right
    /// to pass here. What it must not be read as saying is "this config works":
    /// the default has `api_key: None`, and `provider_key_check_fails_when_the
    /// _active_provider_has_no_key` below pins that separately.
    #[tokio::test]
    async fn config_schema_check_passes_on_default_config() {
        let cfg = Config::default();
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ConfigSchemaCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
    }

    /// The gap this check closes: a fresh install has `default_provider =
    /// openrouter` and no key, and every diagnostic reported healthy while the
    /// agent could not send a single message.
    #[tokio::test]
    async fn provider_key_check_fails_when_the_active_provider_has_no_key() {
        let cfg = Config::default();
        assert!(cfg.api_key.is_none(), "precondition: default has no key");
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ProviderKeyCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail, "msg: {}", result.message);
    }

    /// A key under `provider_api_keys` is what the web console writes. Reading
    /// only the top-level `api_key` — as every diagnostic did — reported such a
    /// config as keyless.
    #[tokio::test]
    async fn provider_key_check_sees_a_key_stored_per_provider() {
        let mut cfg = Config::default();
        cfg.api_key = None;
        cfg.provider_api_keys
            .insert("openrouter".into(), "sk-test".into());
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ProviderKeyCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
    }

    #[tokio::test]
    async fn provider_key_check_accepts_the_top_level_key_for_the_active_provider() {
        let mut cfg = Config::default();
        cfg.api_key = Some("sk-test".into());
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ProviderKeyCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Ok, "msg: {}", result.message);
    }

    /// An empty-string key is not a key. `resolve_key_for_provider` trims and
    /// rejects it; the check must not paper over that.
    #[tokio::test]
    async fn provider_key_check_rejects_a_blank_key() {
        let mut cfg = Config::default();
        cfg.api_key = Some("   ".into());
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ProviderKeyCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail, "msg: {}", result.message);
    }

    /// Local providers need no key — failing them would be a false alarm.
    /// `lmstudio` is deliberate: the catalog marks it `local`, but
    /// `onboard::wizard`'s own keyless list omits it, so this pins the
    /// catalog as the source of truth.
    #[tokio::test]
    async fn provider_key_check_passes_for_local_providers_without_a_key() {
        for name in ["ollama", "llamacpp", "lmstudio"] {
            let mut cfg = Config::default();
            cfg.default_provider = Some(name.into());
            cfg.api_key = None;
            let (ctx, _tmp) = ctx_with_config(cfg);
            let result = ProviderKeyCheck.run(&ctx).await;
            assert_eq!(result.severity, Severity::Ok, "{name}: {}", result.message);
        }
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
