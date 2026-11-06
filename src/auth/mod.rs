pub mod anthropic_token;
pub mod profiles;

use crate::auth::profiles::{
    profile_id, AuthProfile, AuthProfileKind, AuthProfilesData, AuthProfilesStore,
};
use crate::config::Config;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const ANTHROPIC_PROVIDER: &str = "anthropic";
const DEFAULT_PROFILE_NAME: &str = "default";

#[derive(Clone)]
pub struct AuthService {
    store: AuthProfilesStore,
    client: reqwest::Client,
}

impl AuthService {
    pub fn from_config(config: &Config) -> Self {
        let state_dir = state_dir_from_config(config);
        Self::new(&state_dir, config.secrets.encrypt)
    }

    pub fn new(state_dir: &Path, encrypt_secrets: bool) -> Self {
        Self {
            store: AuthProfilesStore::new(state_dir, encrypt_secrets),
            client: reqwest::Client::new(),
        }
    }

    pub async fn load_profiles(&self) -> Result<AuthProfilesData> {
        self.store.load().await
    }

    pub async fn store_provider_token(
        &self,
        provider: &str,
        profile_name: &str,
        token: &str,
        metadata: HashMap<String, String>,
        set_active: bool,
    ) -> Result<AuthProfile> {
        let mut profile = AuthProfile::new_token(provider, profile_name, token.to_string());
        profile.metadata.extend(metadata);
        self.store
            .upsert_profile(profile.clone(), set_active)
            .await?;
        Ok(profile)
    }

    pub async fn set_active_profile(
        &self,
        provider: &str,
        requested_profile: &str,
    ) -> Result<String> {
        let provider = normalize_provider(provider)?;
        let data = self.store.load().await?;
        let profile_id = resolve_requested_profile_id(&provider, requested_profile);

        let profile = data
            .profiles
            .get(&profile_id)
            .ok_or_else(|| anyhow::anyhow!("Auth profile not found: {profile_id}"))?;

        if profile.provider != provider {
            anyhow::bail!(
                "Profile {profile_id} belongs to provider {}, not {}",
                profile.provider,
                provider
            );
        }

        self.store
            .set_active_profile(&provider, &profile_id)
            .await?;
        Ok(profile_id)
    }

    pub async fn remove_profile(&self, provider: &str, requested_profile: &str) -> Result<bool> {
        let provider = normalize_provider(provider)?;
        let profile_id = resolve_requested_profile_id(&provider, requested_profile);
        self.store.remove_profile(&profile_id).await
    }

    pub async fn get_profile(
        &self,
        provider: &str,
        profile_override: Option<&str>,
    ) -> Result<Option<AuthProfile>> {
        let provider = normalize_provider(provider)?;
        let data = self.store.load().await?;
        let Some(profile_id) = select_profile_id(&data, &provider, profile_override) else {
            return Ok(None);
        };
        Ok(data.profiles.get(&profile_id).cloned())
    }

    pub async fn get_provider_bearer_token(
        &self,
        provider: &str,
        profile_override: Option<&str>,
    ) -> Result<Option<String>> {
        let profile = self.get_profile(provider, profile_override).await?;
        let Some(profile) = profile else {
            return Ok(None);
        };

        let credential = match profile.kind {
            AuthProfileKind::Token => profile.token,
            AuthProfileKind::OAuth => profile.token_set.map(|t| t.access_token),
        };

        Ok(credential.filter(|t| !t.trim().is_empty()))
    }
}

pub fn normalize_provider(provider: &str) -> Result<String> {
    let normalized = provider.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "anthropic" | "claude" | "claude-code" => Ok(ANTHROPIC_PROVIDER.to_string()),
        other if !other.is_empty() => Ok(other.to_string()),
        _ => anyhow::bail!("Provider name cannot be empty"),
    }
}

/// Whether an auth profile exists for `provider`, read synchronously.
///
/// `AuthService::get_profile` is async and takes the store lock; this is a
/// presence check on the same file for callers that must stay offline and
/// cheap. `has_usable_credential` is one — `doctor` calls it on a fast path —
/// and it only needs to know whether `rantaiclaw auth login` has been run, not
/// what the token is. No decryption: an encrypted token is still a token.
///
/// Any read failure answers `false`: an unreadable or malformed store is not a
/// credential the provider could send with either.
pub fn has_auth_profile(state_dir: &Path, provider: &str) -> bool {
    let Ok(provider) = normalize_provider(provider) else {
        return false;
    };
    let Ok(bytes) = std::fs::read(state_dir.join(profiles::PROFILES_FILENAME)) else {
        return false;
    };
    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    parsed
        .get("profiles")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|profiles| {
            profiles.values().any(|profile| {
                profile.get("provider").and_then(serde_json::Value::as_str) == Some(&provider)
            })
        })
}

/// The token and `auth_kind` metadata of the anthropic token profile a provider
/// should send, read synchronously for the sync provider factory.
///
/// Selection is `select_profile_id` — the override, then the active profile,
/// then `default` — and it is the same selection `AuthService::get_profile`
/// makes. Only a `token` profile carries an Anthropic credential here. A missing
/// or unreadable store, a profile of another provider, or an empty token answers
/// `None`; the caller then falls through to the env vars, as with no profile.
pub fn anthropic_profile_token(
    state_dir: &Path,
    profile_override: Option<&str>,
) -> Option<(String, Option<String>)> {
    AuthProfilesStore::new(state_dir, true).read_token_sync(ANTHROPIC_PROVIDER, profile_override)
}

pub fn state_dir_from_config(config: &Config) -> PathBuf {
    config
        .config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

pub fn default_profile_id(provider: &str) -> String {
    profile_id(provider, DEFAULT_PROFILE_NAME)
}

fn resolve_requested_profile_id(provider: &str, requested: &str) -> String {
    if requested.contains(':') {
        requested.to_string()
    } else {
        profile_id(provider, requested)
    }
}

pub fn select_profile_id(
    data: &AuthProfilesData,
    provider: &str,
    profile_override: Option<&str>,
) -> Option<String> {
    if let Some(override_profile) = profile_override {
        let requested = resolve_requested_profile_id(provider, override_profile);
        if data.profiles.contains_key(&requested) {
            return Some(requested);
        }
        return None;
    }

    if let Some(active) = data.active_profiles.get(provider) {
        if data.profiles.contains_key(active) {
            return Some(active.clone());
        }
    }

    let default = default_profile_id(provider);
    if data.profiles.contains_key(&default) {
        return Some(default);
    }

    data.profiles
        .iter()
        .find_map(|(id, profile)| (profile.provider == provider).then(|| id.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::profiles::{AuthProfile, AuthProfileKind};

    #[test]
    fn normalize_provider_aliases() {
        assert_eq!(normalize_provider("claude").unwrap(), "anthropic");
        assert_eq!(normalize_provider("openai").unwrap(), "openai");
    }

    #[test]
    fn select_profile_prefers_override_then_active_then_default() {
        let mut data = AuthProfilesData::default();
        let id_active = profile_id("anthropic", "work");
        let id_default = profile_id("anthropic", "default");

        data.profiles.insert(
            id_default.clone(),
            AuthProfile {
                id: id_default.clone(),
                provider: "anthropic".into(),
                profile_name: "default".into(),
                kind: AuthProfileKind::Token,
                account_id: None,
                workspace_id: None,
                token_set: None,
                token: Some("x".into()),
                metadata: std::collections::BTreeMap::default(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );
        data.profiles.insert(
            id_active.clone(),
            AuthProfile {
                id: id_active.clone(),
                provider: "anthropic".into(),
                profile_name: "work".into(),
                kind: AuthProfileKind::Token,
                account_id: None,
                workspace_id: None,
                token_set: None,
                token: Some("y".into()),
                metadata: std::collections::BTreeMap::default(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        );

        data.active_profiles
            .insert("anthropic".into(), id_active.clone());

        assert_eq!(
            select_profile_id(&data, "anthropic", Some("default")),
            Some(id_default)
        );
        assert_eq!(select_profile_id(&data, "anthropic", None), Some(id_active));
    }
}
