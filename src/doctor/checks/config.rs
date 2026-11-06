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
                if let Some(problem) = provider_problem(name) {
                    problems.push(
                        problem.message(&format!("default_provider \"{name}\""), "is invalid"),
                    );
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
            if let Some(problem) = provider_problem(fb) {
                problems.push(problem.message(&format!("fallback provider \"{fb}\""), "invalid"));
            }
        }

        // Per-route providers: nothing else validated `model_routes[].provider`,
        // so a typo'd provider was accepted everywhere and failed only at
        // routing time. A duplicate hint silently shadows, since the router keys
        // on the hint.
        let mut seen_hints = std::collections::HashSet::new();
        for route in &cfg.model_routes {
            if let Some(problem) = provider_problem(&route.provider) {
                problems.push(problem.message(
                    &format!(
                        "model_route \"{}\" provider \"{}\"",
                        route.hint, route.provider
                    ),
                    "invalid",
                ));
            }
            if !seen_hints.insert(route.hint.as_str()) {
                problems.push(format!("duplicate model_route hint \"{}\"", route.hint));
            }
        }
        for route in &cfg.embedding_routes {
            if let Some(problem) = provider_problem(&route.provider) {
                problems.push(problem.message(
                    &format!(
                        "embedding_route \"{}\" provider \"{}\"",
                        route.hint, route.provider
                    ),
                    "invalid",
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

        // Ask the ONE question the send path asks. `resolve_key_for_provider`
        // answers a narrower one — "is a key in config?" — so an install
        // authenticated by environment or by AWS variables was told the agent
        // could not send while it sent fine. `has_usable_credential` covers
        // local providers too, so the separate keyless branch that used to sit
        // here is gone.
        let resolved = ctx.config.resolve_key_for_provider(provider);

        // A setup-token is refused when the request is sent, so the credential
        // RESOLVES and the provider BUILDS: the failure is invisible until a
        // turn is attempted. It is a breakage, not a setup gap, so it fails.
        if let Some(problem) = provider_credential_problem(provider, resolved.as_deref()) {
            return CheckResult::fail(
                self.name(),
                problem.message(&format!("credential for {provider}"), "is refused"),
            )
            .with_category(self.category())
            .with_hint("run: rantaiclaw setup provider");
        }

        match (
            crate::providers::has_usable_credential(provider, resolved.as_deref()),
            resolved,
        ) {
            (true, _) => {
                CheckResult::ok(self.name(), format!("credential resolved for {provider}"))
                    .with_category(self.category())
            }
            // Warn, not Fail: a missing key is a setup GAP, not a breakage — a
            // fresh headless `setup --non-interactive` legitimately leaves it
            // unset, and `doctor --brief` must still exit 0 there (the
            // `setup && doctor` smoke contract, `tests/setup_e2e.rs`). Warn
            // still surfaces it as `⚠` with an actionable hint, which is the
            // whole point — doctor no longer claims a keyless config is `✓`.
            (false, _) => CheckResult::warn(
                self.name(),
                format!(
                    "no credential for {provider} — tried config, its environment variables, \
                     and the provider's own auth (AWS env). \
                     The agent cannot send a message yet"
                ),
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

/// Are the login files an earlier RantaiClaw wrote still on disk?
///
/// The startup sweep removes them, so a file that is still there means the
/// sweep could NOT remove it: a read-only directory, a file owned by someone
/// else, a mount the operator put there. The credential is still on disk, so
/// it is worth saying so — with the paths, which are not secrets, and never
/// with the contents, which are.
pub struct LegacyLoginFilesCheck;

#[async_trait]
impl DoctorCheck for LegacyLoginFilesCheck {
    fn name(&self) -> &'static str {
        "config.legacy_logins"
    }
    fn category(&self) -> &'static str {
        "config"
    }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let present: Vec<String> =
            crate::config::legacy_logins::legacy_login_file_paths(&ctx.config)
                .into_iter()
                .filter(|path| path.exists())
                .map(|path| path.display().to_string())
                .collect();

        if present.is_empty() {
            return CheckResult::ok(self.name(), "no legacy login files on disk")
                .with_category(self.category());
        }

        // Warn, not Fail: the files are harmless to the agent — nothing reads
        // them any more — but a credential left on disk is a credential the
        // operator did not ask to keep. The fix is the operator's to make.
        CheckResult::warn(
            self.name(),
            format!(
                "{} legacy login file(s) could not be removed: {}",
                present.len(),
                present.join(", ")
            ),
        )
        .with_category(self.category())
        .with_hint("delete them by hand, or check the directory's permissions")
    }
}

fn provider_validation_error(name: &str) -> Option<String> {
    // Validate the provider spec's SHAPE (name / custom URL), not credential
    // presence: pass a placeholder key so providers that now require one at
    // construction (e.g. the rig `anthropic-custom:` path) don't report a
    // missing key as an invalid-shape error. Credential presence is checked
    // separately (`ProviderKeyCheck` / `resolve_key_for_provider`).
    match crate::providers::create_provider(name, Some("doctor-shape-check")) {
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

/// What is wrong with a provider name in the config.
///
/// Two kinds, and the difference matters to the operator. A name whose
/// subscription login was REMOVED is still a provider the factory knows, and
/// what it needs is an API key under a different name — so the report names
/// the replacement. A name that never existed is simply wrong, and keeps the
/// wording it always had.
enum ProviderProblem {
    /// The name is one whose subscription login was removed. The sentence
    /// names the API-key replacement.
    RemovedLogin(&'static str),
    /// The name is not one the factory knows.
    Invalid(String),
    /// The credential the provider would send is a Claude subscription
    /// setup-token, which this build refuses.
    SetupToken(&'static str),
}

impl ProviderProblem {
    /// The message for this problem, in the wording of its slot.
    ///
    /// `subject` is the whole subject of the sentence — `default_provider "x"`,
    /// `model_route "hint" provider "x"` — and `invalid` is how that slot has
    /// always worded a provider it does not know.
    fn message(&self, subject: &str, invalid: &str) -> String {
        match self {
            Self::RemovedLogin(sentence) => {
                format!("{subject} names a removed login: {sentence}")
            }
            Self::Invalid(reason) => format!("{subject} {invalid}: {reason}"),
            Self::SetupToken(sentence) => format!("{subject}: {sentence}"),
        }
    }
}

/// The problem with `name`, if it has one.
///
/// A removed login is classified BEFORE the factory is asked: the factory
/// still builds a provider for those names (so the daemon stays up), and
/// asking it would report them as valid.
fn provider_problem(name: &str) -> Option<ProviderProblem> {
    if let Some(sentence) = crate::providers::removed_login_refusal(name) {
        return Some(ProviderProblem::RemovedLogin(sentence));
    }
    provider_validation_error(name).map(ProviderProblem::Invalid)
}

/// The problem with the CREDENTIAL `name` would send, if it has one.
///
/// A setup-token authenticates a Claude subscription, and this build refuses
/// one: the provider still builds (so the gateway binds), and every request is
/// refused. The report names the API key that replaces it.
fn provider_credential_problem(name: &str, config_key: Option<&str>) -> Option<ProviderProblem> {
    crate::providers::refused_credential(name, config_key).map(ProviderProblem::SetupToken)
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
        // This check now asks `has_usable_credential`, which reads the
        // environment; own it, or the verdict depends on the developer's shell.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

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
        // This check now asks `has_usable_credential`, which reads the
        // environment; own it, or the verdict depends on the developer's shell.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let mut cfg = Config::default();
        cfg.api_key = Some("   ".into());
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ProviderKeyCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
    }

    /// `doctor` used to ask a NARROWER question than the send path: "is a key
    /// in config?". An install authenticated by AWS environment variables was
    /// told the agent could not send, while it sent fine. Both checks now ask
    /// `has_usable_credential`, so all three surfaces give one answer.
    #[tokio::test]
    async fn provider_key_check_sees_a_credential_that_is_not_a_config_key() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let mut cfg = Config::default();
        cfg.default_provider = Some("bedrock".into());
        cfg.api_key = None;

        let (ctx, _tmp) = ctx_with_config(cfg.clone());
        assert_eq!(
            ProviderKeyCheck.run(&ctx).await.severity,
            Severity::Warn,
            "no AWS credentials anywhere is genuinely unusable"
        );

        let _id = crate::test_env::EnvGuard::set("AWS_ACCESS_KEY_ID", "AKIAEXAMPLEPLACEHOLDER");
        let _secret =
            crate::test_env::EnvGuard::set("AWS_SECRET_ACCESS_KEY", "neutral-aws-secret-value");

        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ProviderKeyCheck.run(&ctx).await;
        assert_eq!(
            result.severity,
            Severity::Ok,
            "an install that can send must not be reported as keyless: {}",
            result.message
        );
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

    /// A setup-token is a Claude subscription credential this build refuses.
    /// The provider still builds — so the gateway binds and the daemon stays
    /// up — and every request is refused. The check must FAIL: the agent
    /// cannot send, and a Warn would let a broken install look like a setup
    /// gap.
    ///
    /// The message names the API key that replaces it and never the token.
    #[tokio::test]
    async fn provider_key_check_fails_on_a_setup_token_credential() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let mut cfg = Config::default();
        cfg.default_provider = Some("anthropic".into());
        cfg.api_key = None;
        cfg.provider_api_keys.insert(
            "anthropic".into(),
            "sk-ant-oat01-doctor-marker-token".into(),
        );
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ProviderKeyCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail, "msg: {}", result.message);
        assert!(
            result.message.contains("ANTHROPIC_API_KEY"),
            "msg: {}",
            result.message
        );
        assert!(
            !result.message.contains("doctor-marker-token"),
            "the report must not echo the credential: {}",
            result.message
        );
    }

    /// The startup sweep removes the legacy login files, so a file that is
    /// still there means the sweep could NOT remove it: a read-only directory,
    /// a file owned by someone else. The check says so, naming the path —
    /// and never the contents, which are a credential.
    #[tokio::test]
    async fn legacy_login_files_check_warns_when_a_legacy_file_is_still_there() {
        let (ctx, tmp) = ctx_with_config(Config::default());
        let mut config = ctx.config.clone();
        // The state dir is the directory the config file lives in, so the
        // check must read this test's directory, never the operator's own.
        config.config_path = tmp.path().join("config.toml");
        std::fs::write(
            tmp.path().join("auth-profiles.json"),
            "sk-ant-oat01-doctor-marker-token",
        )
        .expect("write the legacy file");
        let ctx = DoctorContext {
            profile: ctx.profile,
            config,
            offline: false,
        };

        let result = LegacyLoginFilesCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
        assert!(
            result.message.contains("auth-profiles.json"),
            "msg: {}",
            result.message
        );
        assert!(
            !result.message.contains("doctor-marker-token"),
            "the report must not echo the credential: {}",
            result.message
        );
    }

    /// The Copilot token pair lives in the PLATFORM config directory, not
    /// beside the config file, so the check must look there too.
    #[tokio::test]
    async fn legacy_login_files_check_sees_the_copilot_files_in_the_platform_config_directory() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let scratch = tempfile::tempdir().expect("scratch dir");
        let xdg = scratch.path().join("xdg");
        let copilot = xdg.join("rantaiclaw").join("copilot");
        std::fs::create_dir_all(&copilot).expect("the copilot dir");
        std::fs::write(copilot.join("access-token"), "copilot-marker-token")
            .expect("write the copilot token");
        let _xdg = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", &xdg);

        let (ctx, tmp) = ctx_with_config(Config::default());
        let mut config = ctx.config.clone();
        config.config_path = tmp.path().join("config.toml");
        let ctx = DoctorContext {
            profile: ctx.profile,
            config,
            offline: false,
        };

        let result = LegacyLoginFilesCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Warn, "msg: {}", result.message);
        assert!(
            result.message.contains("access-token"),
            "msg: {}",
            result.message
        );
        assert!(
            !result.message.contains("copilot-marker-token"),
            "the report must not echo the credential: {}",
            result.message
        );
    }

    /// Nothing left on disk is the normal case, and the check must say so
    /// rather than stay quiet.
    ///
    /// The platform config directory is pointed at an empty one, so the
    /// operator's own files cannot decide the verdict.
    #[tokio::test]
    async fn legacy_login_files_check_passes_when_there_is_nothing_to_sweep() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let scratch = tempfile::tempdir().expect("scratch dir");
        let xdg = scratch.path().join("xdg");
        std::fs::create_dir_all(&xdg).expect("the config home");
        let _xdg = crate::test_env::EnvGuard::set("XDG_CONFIG_HOME", &xdg);

        let (ctx, tmp) = ctx_with_config(Config::default());
        let mut config = ctx.config.clone();
        config.config_path = tmp.path().join("config.toml");
        let ctx = DoctorContext {
            profile: ctx.profile,
            config,
            offline: false,
        };

        let result = LegacyLoginFilesCheck.run(&ctx).await;
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

    /// A config that still names a removed subscription login must be
    /// reported with the API-key replacement, not as an invalid provider:
    /// the name is still a provider, and what it needs is a key.
    #[tokio::test]
    async fn config_schema_check_reports_a_removed_login_with_its_replacement() {
        // This check reads the environment; own it, or the verdict depends on
        // the developer's shell.
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let mut cfg = Config::default();
        cfg.default_provider = Some("openai-codex".into());
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ConfigSchemaCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail, "{}", result.message);
        assert!(
            result.message.contains("names a removed login"),
            "{}",
            result.message
        );
        assert!(
            result.message.contains("OPENAI_API_KEY"),
            "the replacement must be named: {}",
            result.message
        );
    }

    /// Every slot a provider name can sit in reports a removed login the same
    /// way, with the replacement named.
    #[tokio::test]
    async fn config_schema_check_reports_a_removed_login_in_every_slot() {
        let _env = crate::test_env::ENV_LOCK.lock().await;
        let _scrub = crate::test_env::CredentialEnvScrub::new();

        let mut cfg = Config::default();
        cfg.default_provider = Some("openrouter".into());
        cfg.reliability.fallback_providers = vec!["copilot".into()];
        cfg.model_routes = vec![crate::config::schema::ModelRouteConfig {
            hint: "code".into(),
            provider: "qwen-code".into(),
            model: "x".into(),
            api_key: None,
        }];
        cfg.embedding_routes = vec![crate::config::schema::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "minimax-oauth".into(),
            model: "x".into(),
            dimensions: None,
            api_key: None,
        }];
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ConfigSchemaCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail, "{}", result.message);
        for slot in [
            "fallback provider \"copilot\"",
            "model_route \"code\" provider \"qwen-code\"",
            "embedding_route \"semantic\" provider \"minimax-oauth\"",
        ] {
            assert!(
                result
                    .message
                    .contains(&format!("{slot} names a removed login")),
                "{}",
                result.message
            );
        }
        // The replacement is named for the slot that has one.
        assert!(
            result.message.contains("DASHSCOPE_API_KEY"),
            "the Qwen replacement must be named: {}",
            result.message
        );
        assert!(
            result.message.contains("MINIMAX_API_KEY"),
            "the MiniMax replacement must be named: {}",
            result.message
        );
    }

    /// A name that never was a provider is still reported as invalid, in the
    /// wording each slot has always used.
    #[tokio::test]
    async fn config_schema_check_keeps_the_invalid_wording_for_an_unknown_provider() {
        let mut cfg = Config::default();
        cfg.default_provider = Some("totally-fake".into());
        cfg.reliability.fallback_providers = vec!["totally-fake".into()];
        cfg.model_routes = vec![crate::config::schema::ModelRouteConfig {
            hint: "code".into(),
            provider: "totally-fake".into(),
            model: "x".into(),
            api_key: None,
        }];
        cfg.embedding_routes = vec![crate::config::schema::EmbeddingRouteConfig {
            hint: "semantic".into(),
            provider: "totally-fake".into(),
            model: "x".into(),
            dimensions: None,
            api_key: None,
        }];
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ConfigSchemaCheck.run(&ctx).await;
        assert_eq!(result.severity, Severity::Fail, "{}", result.message);
        for slot in [
            "default_provider \"totally-fake\" is invalid:",
            "fallback provider \"totally-fake\" invalid:",
            "model_route \"code\" provider \"totally-fake\" invalid:",
            "embedding_route \"semantic\" provider \"totally-fake\" invalid:",
        ] {
            assert!(result.message.contains(slot), "{}", result.message);
        }
    }

    #[tokio::test]
    async fn provider_requiring_key_is_not_misdiagnosed() {
        // A provider that requires a credential at construction (the rig
        // `anthropic-custom:` path) must validate on SHAPE, not on credential
        // presence — otherwise `config.schema` reports a missing key as
        // "default_provider is invalid". The placeholder key makes it construct.
        let mut cfg = Config::default();
        cfg.default_provider = Some("anthropic-custom:https://api.example.com".into());
        cfg.api_key = None;
        let (ctx, _tmp) = ctx_with_config(cfg);
        let result = ConfigSchemaCheck.run(&ctx).await;
        assert!(
            !result.message.contains("default_provider") || !result.message.contains("is invalid"),
            "a keyless key-requiring provider must not be misdiagnosed as invalid: {}",
            result.message
        );
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
