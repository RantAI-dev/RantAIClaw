pub mod api_url;
pub mod fingerprint;
pub mod legacy_logins;
pub mod migrations;
pub mod persistent;
pub mod schema;
pub mod watcher;

use std::path::PathBuf;

#[allow(unused_imports)]
pub use persistent::PersistentConfigError;

#[allow(unused_imports)]
pub use schema::{
    apply_runtime_proxy_to_blocking_builder, apply_runtime_proxy_to_builder,
    build_runtime_proxy_client, build_runtime_proxy_client_no_redirect,
    build_runtime_proxy_client_with_timeouts, runtime_proxy_config, set_runtime_proxy_config,
    AgentConfig, AuditConfig, AutonomyConfig, BrowserComputerUseConfig, BrowserConfig,
    ChannelsConfig, ClassificationRule, ComposioConfig, Config, CostConfig, CronConfig,
    DelegateAgentConfig, DiscordConfig, DockerRuntimeConfig, EmbeddingRouteConfig,
    GatewayAgentConfig, GatewayConfig, GatewayLoginConfig, HeartbeatConfig, HttpRequestConfig,
    IMessageConfig, IdentityConfig, KnowledgeConfig, LarkConfig, MatrixConfig, MemoryConfig,
    ModelRouteConfig, MultimodalConfig, NextcloudTalkConfig, ObservabilityConfig, ProxyConfig,
    ProxyScope, QueryClassificationConfig, ReliabilityConfig, RuntimeConfig, SchedulerConfig,
    SecretsConfig, SkillApiKey, SkillEntryConfig, SkillsConfig, SkillsInstallConfig,
    SkillsPromptInjectionMode, SlackConfig, StreamMode, TasksConfig, TelegramConfig, TunnelConfig,
    UiConfig, WebSearchConfig, WebhookConfig, WhatsAppWebConfig,
};

/// The directory this install keeps its per-profile state in.
///
/// It is the directory the config file lives in, so the secret store, the
/// model cache and the session database sit beside it. It used to live in the
/// auth module, which no longer exists: it is a config-dir helper, not a
/// login helper.
pub fn state_dir_from_config(config: &Config) -> PathBuf {
    config
        .config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state dir is the directory the config file lives in: every
    /// per-profile file sits beside it.
    #[test]
    fn state_dir_is_the_config_file_directory() {
        let mut config = Config::default();
        config.config_path = std::path::PathBuf::from("/tmp/rantaiclaw-profile/config.toml");
        assert_eq!(
            state_dir_from_config(&config),
            std::path::PathBuf::from("/tmp/rantaiclaw-profile")
        );
    }

    /// A config path with no directory part has no parent directory to name:
    /// the helper returns the empty path, which is what it returned when it
    /// lived in the auth module. The behaviour is unchanged — the helper only
    /// MOVED.
    #[test]
    fn state_dir_of_a_bare_config_path_is_empty() {
        let mut config = Config::default();
        config.config_path = std::path::PathBuf::from("config.toml");
        assert_eq!(state_dir_from_config(&config), std::path::PathBuf::new());
    }

    #[test]
    fn reexported_config_default_is_constructible() {
        let config = Config::default();

        assert!(config.default_provider.is_some());
        // `default_model` is intentionally unset on a fresh install — the agent
        // refuses to guess one, and setup fills it in.
        assert!(config.default_model.is_none());
        assert!(config.default_temperature > 0.0);
    }

    #[test]
    fn reexported_channel_configs_are_constructible() {
        let telegram = TelegramConfig {
            bot_token: "token".into(),
            allowed_users: vec!["alice".into()],
            stream_mode: StreamMode::default(),
            draft_update_interval_ms: 1000,
            interrupt_on_new_message: false,
            mention_only: false,
        };

        let discord = DiscordConfig {
            bot_token: "token".into(),
            guild_id: Some("123".into()),
            allowed_users: vec![],
            listen_to_bots: false,
            mention_only: false,
        };

        let lark = LarkConfig {
            app_id: "app-id".into(),
            app_secret: "app-secret".into(),
            encrypt_key: None,
            verification_token: None,
            allowed_users: vec![],
            use_feishu: false,
            receive_mode: crate::config::schema::LarkReceiveMode::Websocket,
            port: None,
        };

        let nextcloud_talk = NextcloudTalkConfig {
            base_url: "https://cloud.example.com".into(),
            app_token: "app-token".into(),
            webhook_secret: None,
            allowed_users: vec!["*".into()],
        };

        assert_eq!(telegram.allowed_users.len(), 1);
        assert_eq!(discord.guild_id.as_deref(), Some("123"));
        assert_eq!(lark.app_id, "app-id");
        assert_eq!(nextcloud_talk.base_url, "https://cloud.example.com");
    }
}
