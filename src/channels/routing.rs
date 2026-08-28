//! Runtime config reload, per-sender route overrides and the provider cache.
//!
//! Moved out of `mod.rs` verbatim (plan 121, row 5). No behaviour change.
//!
//! The tests for this code stayed in `mod.rs`'s test module, which reaches them
//! through a glob import: they are interleaved with dispatch tests that share
//! the same fixtures, and splitting those fixtures would have made the move
//! stop being a move. The plan allows this — the alternative it names is
//! widening visibility, which is what happened.

use super::{
    effective_channel_message_timeout_secs, ChannelRouteSelection, ChannelRuntimeContext,
    ChannelRuntimeDefaults, CHANNEL_CATALOG, MODEL_CACHE_PREVIEW_LIMIT,
};
use crate::config::Config;
use crate::providers::{self, Provider};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Effective threading switch per channel: the shared default, overridden where
/// a channel declares its own. Mattermost is the only per-channel key today —
/// it predates the shared one and operators have it set.
pub(crate) fn channel_thread_replies(cc: &crate::config::ChannelsConfig) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    for (name, _) in CHANNEL_CATALOG {
        out.insert((*name).to_string(), cc.thread_replies);
    }
    if let Some(c) = cc.mattermost.as_ref() {
        if let Some(override_value) = c.thread_replies {
            out.insert("mattermost".to_string(), override_value);
        }
    }
    out
}

/// Per-channel `mention_only`, keyed like `channel_allowlists`.
///
/// Only the three channels whose config carries the flag appear. Unlike the
/// allowlists this is **not** applied on reload: `mention_only` is passed into
/// the channel constructors and lives inside the channel objects, so applying it
/// live needs a `Channel` trait method, which is a cross-file change this plan
/// does not own. It is tracked here purely so a reload can *tell the operator*
/// that their edit needs a restart instead of reporting success and doing
/// nothing.
pub(crate) fn channel_mention_only(cc: &crate::config::ChannelsConfig) -> HashMap<String, bool> {
    let mut out = HashMap::new();
    if let Some(c) = cc.telegram.as_ref() {
        out.insert("telegram".to_string(), c.mention_only);
    }
    if let Some(c) = cc.discord.as_ref() {
        out.insert("discord".to_string(), c.mention_only);
    }
    if let Some(c) = cc.mattermost.as_ref() {
        out.insert("mattermost".to_string(), c.mention_only.unwrap_or(false));
    }
    out
}

pub(crate) fn channel_allowlists(
    cc: &crate::config::ChannelsConfig,
) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    let mut put = |name: &str, list: Option<&Vec<String>>| {
        if let Some(list) = list {
            out.insert(name.to_string(), list.clone());
        }
    };
    put("telegram", cc.telegram.as_ref().map(|c| &c.allowed_users));
    put("discord", cc.discord.as_ref().map(|c| &c.allowed_users));
    put("slack", cc.slack.as_ref().map(|c| &c.allowed_users));
    put(
        "mattermost",
        cc.mattermost.as_ref().map(|c| &c.allowed_users),
    );
    put("matrix", cc.matrix.as_ref().map(|c| &c.allowed_users));
    put("irc", cc.irc.as_ref().map(|c| &c.allowed_users));
    put("lark", cc.lark.as_ref().map(|c| &c.allowed_users));
    put("dingtalk", cc.dingtalk.as_ref().map(|c| &c.allowed_users));
    put("qq", cc.qq.as_ref().map(|c| &c.allowed_users));
    put(
        "nextcloud_talk",
        cc.nextcloud_talk.as_ref().map(|c| &c.allowed_users),
    );
    put("signal", cc.signal.as_ref().map(|c| &c.allowed_from));
    put("whatsapp", cc.whatsapp.as_ref().map(|c| &c.allowed_numbers));
    put("linq", cc.linq.as_ref().map(|c| &c.allowed_senders));
    put(
        "imessage",
        cc.imessage.as_ref().map(|c| &c.allowed_contacts),
    );
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConfigFileStamp {
    pub(crate) modified: SystemTime,
    pub(crate) len: u64,
}

/// What one channel runtime knows about its config file: the applied state, plus
/// warn-once latches for the two paths that used to fail in total silence.
///
/// The latches live here rather than in a `static` so they cannot couple one
/// test to another — which is the defect that removing the global store fixed.
#[derive(Default)]
pub(crate) struct RuntimeConfigSlot {
    pub(crate) state: Option<RuntimeConfigState>,
    /// Set once `runtime_defaults_snapshot` has reported taking its synthesised
    /// fallback. That fallback hands the model a *guessed* autonomy preset the
    /// gate is not enforcing, so it must be visible — but it is consulted per
    /// message, so it must not be visible once per message.
    pub(crate) fallback_warned: bool,
    /// Set once an unreadable/unstattable config file has been reported. Cleared
    /// on the next successful stat so a later outage is reported again.
    pub(crate) stamp_error_warned: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeConfigState {
    pub(crate) defaults: ChannelRuntimeDefaults,
    pub(crate) last_applied_stamp: Option<ConfigFileStamp>,
    /// Reason the most recent reload could not apply the new provider (e.g. no
    /// usable API key). `Some` means the runtime kept the previous provider.
    pub(crate) last_reload_error: Option<String>,
}

/// The most recent reload failure reason for `config_path`, if the runtime kept
/// the previous provider instead of swapping to a broken one. Exposed so an
/// operator surface can report why a channel didn't follow a provider switch.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn last_reload_error(ctx: &ChannelRuntimeContext) -> Option<String> {
    ctx.runtime_config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .state
        .as_ref()
        .and_then(|s| s.last_reload_error.clone())
}

pub(crate) fn resolve_provider_alias(name: &str) -> Option<String> {
    let candidate = name.trim();
    if candidate.is_empty() {
        return None;
    }

    let providers_list = providers::list_providers();
    for provider in providers_list {
        if provider.name.eq_ignore_ascii_case(candidate)
            || provider
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(candidate))
        {
            return Some(provider.name.to_string());
        }
    }

    None
}

pub(crate) fn resolved_default_provider(config: &Config) -> String {
    config
        .default_provider
        .clone()
        .unwrap_or_else(|| "openrouter".to_string())
}

pub(crate) fn resolved_default_model(config: &Config) -> String {
    // No hardcoded fallback: an unconfigured model stays empty so the agent
    // build refuses it with a clear "no model — run setup" error, rather than a
    // channel silently answering with a guessed model.
    config.default_model.clone().unwrap_or_default()
}

pub(crate) fn runtime_defaults_from_config(config: &Config) -> ChannelRuntimeDefaults {
    ChannelRuntimeDefaults {
        default_provider: resolved_default_provider(config),
        model: resolved_default_model(config),
        temperature: config.default_temperature,
        api_key: config.api_key.clone(),
        api_url: config.api_url.clone(),
        reliability: config.reliability.clone(),
        approval_owners: Arc::new(config.channels_config.approval_owners.clone()),
        guest_gate: Arc::new(crate::approval::GuestGate::new(
            config.autonomy.auto_approve.clone(),
            &config.channels_config.guest_allowed_tools,
            &config.channels_config.guest_allowed_commands,
        )),
        allowed_commands: Arc::new(config.autonomy.allowed_commands.clone()),
        autonomy_level: config.autonomy.level,
        autonomy_preset: crate::approval::policy_writer::preset_for_autonomy(&config.autonomy),
        allowlists: Arc::new(channel_allowlists(&config.channels_config)),
        message_timeout_secs: effective_channel_message_timeout_secs(
            config.channels_config.message_timeout_secs,
        ),
        max_tool_iterations: config.agent.max_tool_iterations,
        auto_save_memory: config.memory.auto_save,
        min_relevance_score: config.memory.min_relevance_score,
        autonomous_tools: config.channels_config.autonomous_tools,
        mention_only: Arc::new(channel_mention_only(&config.channels_config)),
        thread_replies: Arc::new(channel_thread_replies(&config.channels_config)),
    }
}

pub(crate) fn runtime_config_path(ctx: &ChannelRuntimeContext) -> Option<PathBuf> {
    ctx.provider_runtime_options
        .rantaiclaw_dir
        .as_ref()
        .map(|dir| dir.join("config.toml"))
}

pub(crate) fn runtime_defaults_snapshot(ctx: &ChannelRuntimeContext) -> ChannelRuntimeDefaults {
    {
        let mut slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = slot.state.as_ref() {
            return state.defaults.clone();
        }
        // No state loaded: everything below is synthesised. In production
        // `start_channels` always seeds this, so reaching here means the runtime
        // is answering messages against a *guessed* autonomy preset that the
        // live gate is not enforcing. Say so — but once, since this is consulted
        // per message.
        if !slot.fallback_warned {
            slot.fallback_warned = true;
            tracing::warn!(
                "Channel runtime has no loaded config state; falling back to \
                 boot-time context values and a guessed autonomy preset. The \
                 enforced policy is whatever SecurityPolicy holds, not this."
            );
        }
    }

    // Fallback only when nothing has been loaded for this runtime. It is seeded
    // at startup in `start_channels`, so in production the snapshot above is
    // authoritative; this mirrors the startup `ctx` fields for the ad-hoc/test
    // path.
    ChannelRuntimeDefaults {
        default_provider: ctx.default_provider.as_str().to_string(),
        model: ctx.model.as_str().to_string(),
        temperature: ctx.temperature,
        api_key: ctx.api_key.clone(),
        api_url: ctx.api_url.clone(),
        reliability: (*ctx.reliability).clone(),
        approval_owners: Arc::clone(&ctx.approval_owners),
        guest_gate: Arc::clone(&ctx.guest_gate),
        allowed_commands: Arc::new(Vec::new()),
        // Empty on the fallback path: this branch has no config to read
        // allowlists from, and an empty map means "apply nothing", so every
        // channel keeps the list it was constructed with. Inventing entries
        // here would let a fallback *widen* a gate, which is the opposite of
        // what a fallback should be able to do.
        allowlists: Arc::new(HashMap::new()),
        // Behaviour knobs mirror the boot-time `ctx`, which is exactly what this
        // fallback is for. Unlike the gate-bearing fields above these carry no
        // authority, so mirroring them cannot widen anything.
        message_timeout_secs: ctx.message_timeout_secs,
        max_tool_iterations: ctx.max_tool_iterations,
        auto_save_memory: ctx.auto_save_memory,
        min_relevance_score: ctx.min_relevance_score,
        // The fallback must not be the permissive answer: `false` keeps the gate
        // armed. A path with no config to read may not decide that tools run
        // unattended.
        autonomous_tools: false,
        // Empty: with no config to compare against, the reload has nothing to
        // report a divergence from.
        mention_only: Arc::new(HashMap::new()),
        // Empty means "no entry", which the lookup reads as the shipped default
        // (threading on). This carries no authority, so mirroring the default
        // cannot widen anything.
        thread_replies: Arc::new(HashMap::new()),
        autonomy_level: ctx.security.effective_autonomy(),
        // Fallback path only (the store has no entry — ad-hoc/tests). The
        // live policy carries the enforced level but not `always_ask`, which
        // is what separates Manual from Smart, so Supervised resolves to the
        // stricter of the two. Production goes through
        // `runtime_defaults_from_config`, which has the full config and
        // resolves the preset exactly.
        autonomy_preset: match ctx.security.effective_autonomy() {
            crate::security::AutonomyLevel::ReadOnly => {
                crate::approval::policy_writer::PolicyPreset::Strict
            }
            crate::security::AutonomyLevel::Full => {
                crate::approval::policy_writer::PolicyPreset::Off
            }
            crate::security::AutonomyLevel::Supervised => {
                crate::approval::policy_writer::PolicyPreset::Manual
            }
        },
    }
}

/// Current approval owners from the live runtime-defaults store (or the startup
/// `ctx` fallback). Mirrors `runtime_defaults_snapshot` so `/approve` / `/allow`
/// reply authorization tracks owner changes without a `channels run` restart.
/// Whether replies on `channel` should thread. Missing entry ⇒ the shipped
/// default (`true`), which is what the fallback defaults path produces.
pub(crate) fn thread_replies_enabled(ctx: &ChannelRuntimeContext, channel: &str) -> bool {
    runtime_defaults_snapshot(ctx)
        .thread_replies
        .get(channel)
        .copied()
        .unwrap_or(true)
}

pub(crate) fn live_approval_owners(ctx: &ChannelRuntimeContext) -> Arc<Vec<String>> {
    runtime_defaults_snapshot(ctx).approval_owners
}

/// Stat the config file for change detection.
///
/// Returns `Result` rather than `Option` because both failures used to be
/// swallowed by `.ok()?` and the caller then returned success with nothing
/// logged — the atomic temp-file-and-rename write this project uses makes a
/// briefly-absent config a real occurrence, and an operator whose edit never
/// applied had no way to find out why.
pub(crate) async fn config_file_stamp(path: &Path) -> Result<ConfigFileStamp> {
    let metadata = tokio::fs::metadata(path)
        .await
        .with_context(|| format!("Failed to stat {}", path.display()))?;
    let modified = metadata
        .modified()
        .with_context(|| format!("No modification time for {}", path.display()))?;
    Ok(ConfigFileStamp {
        modified,
        len: metadata.len(),
    })
}

pub(crate) async fn load_runtime_defaults_from_config_file(
    path: &Path,
) -> Result<(ChannelRuntimeDefaults, crate::config::AutonomyConfig)> {
    // Use the shared, side-effect-free loader so this reload path decrypts EVERY
    // secret (not just `api_key`) and runs the migration chain — it previously
    // hand-decrypted only `config.api_key` against a raw `toml::from_str`, leaving
    // provider/knowledge/telegram secrets `enc2:`-prefixed and skipping migration.
    let mut parsed = Config::load_from_path(path).await?;
    parsed.apply_env_overrides();
    // Hand back the whole `[autonomy]` section, not just the two fields the
    // couriers used to carry: `apply_config` refreshes all eight at once.
    let autonomy = parsed.autonomy.clone();
    Ok((runtime_defaults_from_config(&parsed), autonomy))
}

pub(crate) async fn maybe_apply_runtime_config_update(ctx: &ChannelRuntimeContext) -> Result<()> {
    let Some(config_path) = runtime_config_path(ctx) else {
        return Ok(());
    };

    let stamp = match config_file_stamp(&config_path).await {
        Ok(stamp) => {
            // Recovered: re-arm the latch so a later outage is reported again.
            let mut slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
            slot.stamp_error_warned = false;
            stamp
        }
        Err(err) => {
            // Cannot tell whether the config changed, so nothing is applied.
            // This used to return `Ok(())` silently, which is indistinguishable
            // from "nothing to do" — an operator whose edit never landed saw no
            // reason at all. The stamp is deliberately NOT advanced, so a later
            // successful read still applies the edit.
            let mut slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
            if !slot.stamp_error_warned {
                slot.stamp_error_warned = true;
                tracing::warn!(
                    path = %config_path.display(),
                    "Cannot stat the channel config file, so config changes are not being \
                     applied: {err:#}"
                );
            }
            return Ok(());
        }
    };

    {
        let slot = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = slot.state.as_ref() {
            if state.last_applied_stamp == Some(stamp) {
                return Ok(());
            }
        }
    }

    let (next_defaults, next_autonomy) =
        load_runtime_defaults_from_config_file(&config_path).await?;
    // Snapshot the currently-applied defaults BEFORE we overwrite the store, so we
    // can tell whether the operator actually changed the provider/model.
    let prev_defaults = runtime_defaults_snapshot(ctx);

    // Apply the non-provider settings FIRST, before trying to build the new
    // provider. They don't depend on the provider, and a safety-motivated
    // autonomy downgrade (or command-allowlist change) must take effect even when
    // the — often unrelated — new provider can't be built. Applying them only on
    // the success path meant `rantaiclaw autonomy off` bundled with a broken
    // provider silently never applied.
    //
    // Swap the whole config half in one write. This previously patched only two
    // of the eight `[autonomy]` fields, via per-field override slots; the other
    // six — forbidden_paths, workspace_only, block_high_risk_commands,
    // require_approval_for_medium_risk, and the two budgets — stayed frozen at
    // whatever was on disk when the daemon started. Operator grants in
    // `runtime_allowlist` (`/allow <cmd> --persist`), the rate-limit window and
    // the approval registry are process state and are deliberately untouched.
    ctx.security.apply_config(&next_autonomy);

    // `mention_only` is constructor-injected into the channel objects, so this
    // reload cannot apply it. Saying nothing would repeat the bug this plan
    // exists to fix — "Applied updated channel runtime config from disk" while
    // the edit did nothing — so name the channel and state that a restart is
    // required. Compared against the previously *applied* snapshot, so the
    // warning fires once per edit rather than on every reload.
    for (name, next_value) in next_defaults.mention_only.iter() {
        let previous = prev_defaults.mention_only.get(name.as_str());
        if previous.is_some_and(|prev| prev != next_value) {
            tracing::warn!(
                channel = %name,
                mention_only = *next_value,
                "mention_only changed on disk but cannot be applied to a running \
                 channel — restart the channel runtime for it to take effect"
            );
        }
    }

    // Push per-channel allowlists into the live channel handles, for the same
    // reason the autonomy swap above happens here: an allowlist change is
    // safety-relevant, so it must apply even when the — usually unrelated — new
    // provider cannot be built. Doing it on the success path only would mean a
    // tightened allowlist silently waited on an API key.
    //
    // Channels that hold their allowlist as a plain `Vec` inherit the no-op
    // default on `Channel::apply_allowed_senders` and keep their boot-time list.
    for (name, allowed) in next_defaults.allowlists.iter() {
        if let Some(channel) = ctx.channels_by_name.get(name.as_str()) {
            channel.apply_allowed_senders(allowed);
        }
    }

    let next_default_provider = match providers::create_resilient_provider_with_options(
        &next_defaults.default_provider,
        next_defaults.api_key.as_deref(),
        next_defaults.api_url.as_deref(),
        &next_defaults.reliability,
        &ctx.provider_runtime_options,
    ) {
        Ok(p) => p,
        Err(err) => {
            // Can't build the new provider (e.g. no usable API key). Keep the
            // working provider + previously-applied defaults; advance the stamp so
            // we don't rebuild-and-fail on every message; record the reason so an
            // operator surface can report it. The operator's fix is itself a config
            // write, which changes the stamp and re-triggers this reload.
            let reason = format!("provider '{}': {err}", next_defaults.default_provider);
            tracing::warn!(
                provider = %next_defaults.default_provider,
                "Config reload kept the previous provider — could not build the new one: {err}"
            );
            let mut guard = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
            let entry = guard.state.get_or_insert_with(|| RuntimeConfigState {
                defaults: prev_defaults.clone(),
                last_applied_stamp: None,
                last_reload_error: None,
            });
            // Keeping the old provider must not also keep the old *policy*.
            //
            // This applies the whole reloaded config and then puts back only the
            // fields that genuinely depend on the provider we failed to build.
            // The inversion is the point: an include-list freezes every field
            // added to `ChannelRuntimeDefaults` in future unless someone
            // remembers to extend it, and forgetting is silent. It had already
            // happened — `approval_owners`, `guest_gate` and `allowlists` were
            // dropped here, so removing a compromised owner in the same edit
            // that left the provider unbuildable persisted the removal to disk
            // and never applied it, and the stamp advanced so nothing retried.
            //
            // With the exclusion list, a new field applies by default and
            // freezing one is a deliberate act that has to be written down here.
            let mut applied = next_defaults.clone();
            applied.default_provider = entry.defaults.default_provider.clone();
            applied.model = entry.defaults.model.clone();
            applied.api_key = entry.defaults.api_key.clone();
            applied.api_url = entry.defaults.api_url.clone();
            applied.reliability = entry.defaults.reliability.clone();
            entry.defaults = applied;
            entry.last_applied_stamp = Some(stamp);
            entry.last_reload_error = Some(reason);
            return Ok(());
        }
    };
    let next_default_provider: Arc<dyn Provider> = Arc::from(next_default_provider);

    if let Err(err) = next_default_provider.warmup().await {
        tracing::warn!(
            provider = %next_defaults.default_provider,
            "Provider warmup failed after config reload: {err}"
        );
    }

    {
        let mut cache = ctx.provider_cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.clear();
        cache.insert(
            next_defaults.default_provider.clone(),
            Arc::clone(&next_default_provider),
        );
    }

    // (autonomy level + allowed_commands were already applied above, before the
    // provider build, so they take effect even on the keep-old-provider branch.)
    {
        let mut guard = ctx.runtime_config.lock().unwrap_or_else(|e| e.into_inner());
        guard.state = Some(RuntimeConfigState {
            defaults: next_defaults.clone(),
            last_applied_stamp: Some(stamp),
            last_reload_error: None,
        });
    }

    // If the operator changed the provider or default model (Web-UI switch or a
    // direct config edit), clear per-sender route overrides so senders pinned by
    // an in-chat `/model` / `/models` re-base to the new default — the operator
    // switch wins. Only clear on an actual change, never on unrelated reloads.
    if prev_defaults.default_provider != next_defaults.default_provider
        || prev_defaults.model != next_defaults.model
    {
        let mut routes = ctx
            .route_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !routes.is_empty() {
            tracing::info!(
                cleared = routes.len(),
                provider = %next_defaults.default_provider,
                model = %next_defaults.model,
                "Cleared per-sender route overrides after a provider/model change"
            );
            routes.clear();
        }
    }

    tracing::info!(
        path = %config_path.display(),
        provider = %next_defaults.default_provider,
        model = %next_defaults.model,
        temperature = next_defaults.temperature,
        "Applied updated channel runtime config from disk"
    );

    Ok(())
}

pub(crate) fn default_route_selection(ctx: &ChannelRuntimeContext) -> ChannelRouteSelection {
    let defaults = runtime_defaults_snapshot(ctx);
    ChannelRouteSelection {
        provider: defaults.default_provider,
        model: defaults.model,
    }
}

/// Look up a sender's pinned route, falling back to the current defaults.
///
/// **Lock-order invariant: the runtime-config store is acquired BEFORE
/// `route_overrides`, never while holding it.**
///
/// `default_route_selection` reaches the global config store via
/// `runtime_defaults_snapshot`. Written as one expression, the `route_overrides`
/// guard is a temporary that lives to the end of the statement — so the fallback
/// ran while still holding it, taking the two locks in the opposite order from
/// `set_route_selection` directly below. Both are `std::sync::Mutex` held inside
/// async tasks, so a cycle would wedge the entire dispatch loop with no error
/// and no recovery short of a restart. Binding the lookup drops the guard first.
pub(crate) fn get_route_selection(
    ctx: &ChannelRuntimeContext,
    sender_key: &str,
) -> ChannelRouteSelection {
    let existing = {
        ctx.route_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(sender_key)
            .cloned()
    };
    existing.unwrap_or_else(|| default_route_selection(ctx))
}

pub(crate) fn set_route_selection(
    ctx: &ChannelRuntimeContext,
    sender_key: &str,
    next: ChannelRouteSelection,
) {
    let default_route = default_route_selection(ctx);
    let mut routes = ctx
        .route_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if next == default_route {
        routes.remove(sender_key);
    } else {
        routes.insert(sender_key.to_string(), next);
    }
}

/// Top model IDs for a provider, for the in-channel `/model` reply.
///
/// Resolves through the shared catalog rather than re-reading
/// `models_cache.json` here. This module used to carry its own copy of the
/// cache path, the deserialization structs and the lookup — a fourth reader of
/// one file with a fourth copy of the rules, which is the duplication that let
/// the catalog surfaces drift apart in the first place.
pub(crate) fn load_cached_model_preview(workspace_dir: &Path, provider_name: &str) -> Vec<String> {
    crate::onboard::wizard::provider_model_catalog(workspace_dir, provider_name)
        .models
        .into_iter()
        .take(MODEL_CACHE_PREVIEW_LIMIT)
        .collect()
}

pub(crate) async fn get_or_create_provider(
    ctx: &ChannelRuntimeContext,
    provider_name: &str,
) -> anyhow::Result<Arc<dyn Provider>> {
    if let Some(existing) = ctx
        .provider_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(provider_name)
        .cloned()
    {
        return Ok(existing);
    }

    if provider_name == ctx.default_provider.as_str() {
        return Ok(Arc::clone(&ctx.provider));
    }

    let defaults = runtime_defaults_snapshot(ctx);
    let api_url = if provider_name == defaults.default_provider.as_str() {
        defaults.api_url.as_deref()
    } else {
        None
    };

    let provider = create_resilient_provider_nonblocking(
        provider_name,
        ctx.api_key.clone(),
        api_url.map(ToString::to_string),
        ctx.reliability.as_ref().clone(),
        ctx.provider_runtime_options.clone(),
    )
    .await?;
    let provider: Arc<dyn Provider> = Arc::from(provider);

    if let Err(err) = provider.warmup().await {
        tracing::warn!(provider = provider_name, "Provider warmup failed: {err}");
    }

    let mut cache = ctx.provider_cache.lock().unwrap_or_else(|e| e.into_inner());
    let cached = cache
        .entry(provider_name.to_string())
        .or_insert_with(|| Arc::clone(&provider));
    Ok(Arc::clone(cached))
}

pub(crate) async fn create_resilient_provider_nonblocking(
    provider_name: &str,
    api_key: Option<String>,
    api_url: Option<String>,
    reliability: crate::config::ReliabilityConfig,
    provider_runtime_options: providers::ProviderRuntimeOptions,
) -> anyhow::Result<Box<dyn Provider>> {
    let provider_name = provider_name.to_string();
    tokio::task::spawn_blocking(move || {
        providers::create_resilient_provider_with_options(
            &provider_name,
            api_key.as_deref(),
            api_url.as_deref(),
            &reliability,
            &provider_runtime_options,
        )
    })
    .await
    .context("failed to join provider initialization task")?
}
