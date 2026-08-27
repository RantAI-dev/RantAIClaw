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

        // Per-route providers: nothing else validated `model_routes[].provider`,
        // so a typo'd provider was accepted everywhere and failed only at
        // routing time. A duplicate hint silently shadows, since the router keys
        // on the hint.
        let mut seen_hints = std::collections::HashSet::new();
        for route in &cfg.model_routes {
            if let Some(reason) = provider_validation_error(&route.provider) {
                problems.push(format!(
                    "model_route \"{}\" provider \"{}\" invalid: {reason}",
                    route.hint, route.provider
                ));
            }
            if !seen_hints.insert(route.hint.as_str()) {
                problems.push(format!("duplicate model_route hint \"{}\"", route.hint));
            }
        }
        for route in &cfg.embedding_routes {
            if let Some(reason) = provider_validation_error(&route.provider) {
                problems.push(format!(
                    "embedding_route \"{}\" provider \"{}\" invalid: {reason}",
                    route.hint, route.provider
                ));
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
            // Warn, not Fail: a missing key is a setup GAP, not a breakage — a
            // fresh headless `setup --non-interactive` legitimately leaves it
            // unset, and `doctor --brief` must still exit 0 there (the
            // `setup && doctor` smoke contract, `tests/setup_e2e.rs`). Warn
            // still surfaces it as `⚠` with an actionable hint, which is the
            // whole point — doctor no longer claims a keyless config is `✓`.
            None => CheckResult::warn(
                self.name(),
                format!("no API key for {provider} — the agent cannot send a message yet"),
            )
            .with_category(self.category())
            .with_hint("run: rantaiclaw setup provider"),
        }
    }
}

/// Is `api_url` a URL at all?
///
/// Nothing else reports this shape. `ConfigSchemaCheck` never looked at the
/// field, and the gateway only validates values arriving over its own API — a
/// value typed into `config.toml` by hand, or carried in from an older install,
/// is used as a base URL unexamined.
///
/// Credential-shaped values are not this check's job: [`Config::load_or_init`]
/// drops those at load and tells the operator to rotate the key, so by the time
/// any check runs they are gone. What survives is the harmless-but-wrong case —
/// a typo, a bare hostname, a non-HTTP scheme — which is kept precisely so it
/// can be reported here rather than silently discarded.
pub struct ApiUrlCheck;

#[async_trait]
impl DoctorCheck for ApiUrlCheck {
    fn name(&self) -> &'static str {
        "config.api_url"
    }
    fn category(&self) -> &'static str {
        "config"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let Some(api_url) = ctx
            .config
            .api_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return CheckResult::ok(self.name(), "no api_url override set")
                .with_category(self.category());
        };

        match crate::config::api_url::validate_api_url(api_url) {
            Ok(()) => CheckResult::ok(self.name(), format!("api_url is a valid URL ({api_url})"))
                .with_category(self.category()),
            // Warn, not Fail: most providers ignore `api_url` entirely, so a bad
            // value here breaks nothing for them. It is still wrong, and for the
            // providers that do honour it (llama.cpp, remote Ollama) it is the
            // difference between reaching the server and not.
            Err(reason) => CheckResult::warn(self.name(), format!("api_url is unusable: {reason}"))
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
        // The probe creates, writes, and deletes a file — blocking FS work that
        // must not run on the async runtime.
        let ws_owned = ws.to_path_buf();
        let probe = tokio::task::spawn_blocking(move || writable_probe(&ws_owned))
            .await
            .unwrap_or_else(|e| Err(std::io::Error::other(format!("probe task panicked: {e}"))));
        match probe {
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

    #[tokio::test]
    async fn api_url_check_passes_when_unset_or_a_real_url() {
        let cfg = Config::default();
        assert!(
            cfg.api_url.is_none(),
            "precondition: default sets no api_url"
        );
        let (ctx, _tmp) = ctx_with_config(cfg);
        assert_eq!(ApiUrlCheck.run(&ctx).await.severity, Severity::Ok);

        let mut cfg = Config::default();
        cfg.api_url = Some("http://localhost:8080/v1".into());
        let (ctx, _tmp) = ctx_with_config(cfg);
        assert_eq!(ApiUrlCheck.run(&ctx).await.severity, Severity::Ok);
    }

    /// The case the field is kept for: a value that is wrong but not secret is
    /// left in place at load, so something has to say it is wrong.
    #[tokio::test]
    async fn api_url_check_warns_on_a_value_that_is_not_a_url() {
        for bad in ["not-a-url", "ftp://api.example.com", "api.example.com"] {
            let mut cfg = Config::default();
            cfg.api_url = Some(bad.into());
            let (ctx, _tmp) = ctx_with_config(cfg);

            let result = ApiUrlCheck.run(&ctx).await;
            assert_eq!(
                result.severity,
                Severity::Warn,
                "{bad} should be reported, got: {}",
                result.message
            );
        }
    }

    /// A whitespace-only override is an empty override, not a broken URL —
    /// warning about it would be noise on a config that is doing nothing wrong.
    #[tokio::test]
    async fn api_url_check_treats_a_blank_override_as_unset() {
        let mut cfg = Config::default();
        cfg.api_url = Some("   ".into());
        let (ctx, _tmp) = ctx_with_config(cfg);

        assert_eq!(ApiUrlCheck.run(&ctx).await.severity, Severity::Ok);
    }

    /// The gap this check closes: a fresh install has `default_provider =
    /// openrouter` and no key, and every diagnostic reported healthy while the
    /// agent could not send a single message. It surfaces as Warn (a setup
    /// gap), not Fail — so `doctor --brief` after a headless setup still exits
    /// 0 — but it is no longer `✓`.
    #[tokio::test]
    async fn provider_key_check_warns_when_the_active_provider_has_no_key() {
        let cfg = Config::default();
        assert!(cfg.api_key.is_none(), "precondition: default has no key");
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ProviderKeyCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
        assert!(result.hint.is_some(), "must hint at setup provider");
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
    /// rejects it; the check must not paper over that (warns like any missing
    /// key, does not report `✓`).
    #[tokio::test]
    async fn provider_key_check_rejects_a_blank_key() {
        let mut cfg = Config::default();
        cfg.api_key = Some("   ".into());
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ProviderKeyCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
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
    async fn typo_provider_in_route_is_flagged() {
        let mut cfg = Config::default();
        cfg.default_provider = Some("openrouter".into());
        cfg.model_routes = vec![crate::config::schema::ModelRouteConfig {
            hint: "code".into(),
            provider: "totally-fake".into(),
            model: "x".into(),
            api_key: None,
        }];
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ConfigSchemaCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail, "{}", result.message);
        assert!(result.message.contains("model_route"), "{}", result.message);
    }

    #[tokio::test]
    async fn duplicate_model_route_hint_is_flagged() {
        let route = |p: &str| crate::config::schema::ModelRouteConfig {
            hint: "code".into(),
            provider: p.into(),
            model: "x".into(),
            api_key: None,
        };
        let mut cfg = Config::default();
        cfg.default_provider = Some("openrouter".into());
        cfg.model_routes = vec![route("openrouter"), route("anthropic")];
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ConfigSchemaCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail, "{}", result.message);
        assert!(result.message.contains("duplicate"), "{}", result.message);
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
