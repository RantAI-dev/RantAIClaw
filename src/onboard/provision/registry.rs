//! Provisioner factory registry.

use super::traits::{ProvisionResponse, TuiProvisioner};

pub fn provisioner_for(name: &str) -> Option<Box<dyn TuiProvisioner>> {
    match name {
        // Core
        "persona" => Some(Box::new(super::persona::PersonaProvisioner::new())),
        "provider" => Some(Box::new(super::provider::ProviderProvisioner::new())),
        "approvals" => Some(Box::new(super::approvals::ApprovalsProvisioner::new())),
        "mcp" => Some(Box::new(super::mcp::McpProvisioner::new())),
        "skills" => Some(Box::new(super::skills::SkillsProvisioner::new())),
        // Channels
        "telegram" => Some(Box::new(
            super::channels::telegram::TelegramProvisioner::new(),
        )),
        "discord" => Some(Box::new(super::channels::discord::DiscordProvisioner::new())),
        "slack" => Some(Box::new(super::channels::slack::SlackProvisioner::new())),
        "signal" => Some(Box::new(super::channels::signal::SignalProvisioner::new())),
        "matrix" => Some(Box::new(super::channels::matrix::MatrixProvisioner::new())),
        "mattermost" => Some(Box::new(
            super::channels::mattermost::MattermostProvisioner::new(),
        )),
        "imessage" => Some(Box::new(
            super::channels::imessage::IMessageProvisioner::new(),
        )),
        "lark" => Some(Box::new(super::channels::lark::LarkProvisioner::new())),
        "dingtalk" => Some(Box::new(
            super::channels::dingtalk::DingTalkProvisioner::new(),
        )),
        "nextcloud-talk" => Some(Box::new(
            super::channels::nextcloud_talk::NextcloudTalkProvisioner::new(),
        )),
        "qq" => Some(Box::new(super::channels::qq::QqProvisioner::new())),
        "email" => Some(Box::new(super::channels::email::EmailProvisioner::new())),
        "irc" => Some(Box::new(super::channels::irc::IrcProvisioner::new())),
        "linq" => Some(Box::new(super::channels::linq::LinqProvisioner::new())),
        "whatsapp-cloud" => Some(Box::new(
            super::channels::whatsapp_cloud::WhatsAppCloudProvisioner::new(),
        )),
        // Runtime surfaces
        "memory" => Some(Box::new(
            super::runtime_surfaces::memory::MemoryProvisioner::new(),
        )),
        "runtime" => Some(Box::new(
            super::runtime_surfaces::runtime::RuntimeProvisioner::new(),
        )),
        "proxy" => Some(Box::new(
            super::runtime_surfaces::proxy::ProxyProvisioner::new(),
        )),
        "tunnel" => Some(Box::new(
            super::runtime_surfaces::tunnel::TunnelProvisioner::new(),
        )),
        "gateway" => Some(Box::new(
            super::runtime_surfaces::gateway::GatewayProvisioner::new(),
        )),
        "browser" => Some(Box::new(
            super::runtime_surfaces::browser::BrowserProvisioner::new(),
        )),
        "web-search" => Some(Box::new(
            super::runtime_surfaces::web_search::WebSearchProvisioner::new(),
        )),
        "composio" => Some(Box::new(
            super::runtime_surfaces::composio::ComposioProvisioner::new(),
        )),
        "agents" => Some(Box::new(
            super::runtime_surfaces::agents::AgentsProvisioner::new(),
        )),
        "model-routes" => Some(Box::new(
            super::runtime_surfaces::model_routes::ModelRoutesProvisioner::new(),
        )),
        "embedding-routes" => Some(Box::new(
            super::runtime_surfaces::embedding_routes::EmbeddingRoutesProvisioner::new(),
        )),
        "secrets" => Some(Box::new(
            super::runtime_surfaces::secrets::SecretsProvisioner::new(),
        )),
        "multimodal" => Some(Box::new(
            super::runtime_surfaces::multimodal::MultimodalProvisioner::new(),
        )),
        "hardware" => Some(Box::new(
            super::runtime_surfaces::hardware::HardwareProvisioner::new(),
        )),
        // Feature-gated
        #[cfg(feature = "whatsapp-web")]
        "whatsapp-web" => Some(Box::new(
            super::whatsapp_web::WhatsAppWebProvisioner::default(),
        )),
        _ => None,
    }
}

pub fn available() -> Vec<(&'static str, &'static str)> {
    let mut list = Vec::new();
    // Core
    list.push((
        super::persona::PERSONA_NAME,
        super::persona::PersonaProvisioner::new().description(),
    ));
    list.push((
        super::provider::PROVIDER_NAME,
        super::provider::ProviderProvisioner::new().description(),
    ));
    list.push((
        super::approvals::APPROVALS_NAME,
        super::approvals::ApprovalsProvisioner::new().description(),
    ));
    list.push((
        super::mcp::MCP_NAME,
        super::mcp::McpProvisioner::new().description(),
    ));
    list.push((
        super::skills::SKILLS_NAME,
        super::skills::SkillsProvisioner::new().description(),
    ));
    // Channels
    list.push((
        super::channels::telegram::TELEGRAM_NAME,
        super::channels::telegram::TelegramProvisioner::new().description(),
    ));
    list.push((
        super::channels::discord::DISCORD_NAME,
        super::channels::discord::DiscordProvisioner::new().description(),
    ));
    list.push((
        super::channels::slack::SLACK_NAME,
        super::channels::slack::SlackProvisioner::new().description(),
    ));
    list.push((
        super::channels::signal::SIGNAL_NAME,
        super::channels::signal::SignalProvisioner::new().description(),
    ));
    list.push((
        super::channels::matrix::MATRIX_NAME,
        super::channels::matrix::MatrixProvisioner::new().description(),
    ));
    list.push((
        super::channels::mattermost::MATTERMOST_NAME,
        super::channels::mattermost::MattermostProvisioner::new().description(),
    ));
    list.push((
        super::channels::imessage::IMESSAGE_NAME,
        super::channels::imessage::IMessageProvisioner::new().description(),
    ));
    list.push((
        super::channels::lark::LARK_NAME,
        super::channels::lark::LarkProvisioner::new().description(),
    ));
    list.push((
        super::channels::dingtalk::DINGTALK_NAME,
        super::channels::dingtalk::DingTalkProvisioner::new().description(),
    ));
    list.push((
        super::channels::nextcloud_talk::NEXTCLOUD_TALK_NAME,
        super::channels::nextcloud_talk::NextcloudTalkProvisioner::new().description(),
    ));
    list.push((
        super::channels::qq::QQ_NAME,
        super::channels::qq::QqProvisioner::new().description(),
    ));
    list.push((
        super::channels::email::EMAIL_NAME,
        super::channels::email::EmailProvisioner::new().description(),
    ));
    list.push((
        super::channels::irc::IRC_NAME,
        super::channels::irc::IrcProvisioner::new().description(),
    ));
    list.push((
        super::channels::linq::LINQ_NAME,
        super::channels::linq::LinqProvisioner::new().description(),
    ));
    list.push((
        super::channels::whatsapp_cloud::WHATSAPP_CLOUD_NAME,
        super::channels::whatsapp_cloud::WhatsAppCloudProvisioner::new().description(),
    ));
    // Runtime surfaces
    list.push((
        super::runtime_surfaces::memory::MEMORY_NAME,
        super::runtime_surfaces::memory::MemoryProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::runtime::RUNTIME_NAME,
        super::runtime_surfaces::runtime::RuntimeProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::proxy::PROXY_NAME,
        super::runtime_surfaces::proxy::ProxyProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::tunnel::TUNNEL_NAME,
        super::runtime_surfaces::tunnel::TunnelProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::gateway::GATEWAY_NAME,
        super::runtime_surfaces::gateway::GatewayProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::browser::BROWSER_NAME,
        super::runtime_surfaces::browser::BrowserProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::web_search::WEB_SEARCH_NAME,
        super::runtime_surfaces::web_search::WebSearchProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::composio::COMPOSIO_NAME,
        super::runtime_surfaces::composio::ComposioProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::agents::AGENTS_NAME,
        super::runtime_surfaces::agents::AgentsProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::model_routes::MODEL_ROUTES_NAME,
        super::runtime_surfaces::model_routes::ModelRoutesProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::embedding_routes::EMBEDDING_ROUTES_NAME,
        super::runtime_surfaces::embedding_routes::EmbeddingRoutesProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::secrets::SECRETS_NAME,
        super::runtime_surfaces::secrets::SecretsProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::multimodal::MULTIMODAL_NAME,
        super::runtime_surfaces::multimodal::MultimodalProvisioner::new().description(),
    ));
    list.push((
        super::runtime_surfaces::hardware::HARDWARE_NAME,
        super::runtime_surfaces::hardware::HardwareProvisioner::new().description(),
    ));
    // Feature-gated
    #[cfg(feature = "whatsapp-web")]
    {
        list.push((
            super::whatsapp_web::WHATSAPP_WEB_NAME,
            super::whatsapp_web::WhatsAppWebProvisioner::default().description(),
        ));
    }
    list
}

pub fn test_responses_for(name: &str) -> Vec<ProvisionResponse> {
    match name {
        "persona" => vec![
            ProvisionResponse::Selection(vec![0]),
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Selection(vec![0]),
            ProvisionResponse::Text(String::new()),
        ],
        "provider" => vec![
            ProvisionResponse::Selection(vec![0]),
            ProvisionResponse::Selection(vec![0]),
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Text(String::new()),
        ],
        "approvals" => vec![ProvisionResponse::Selection(vec![0])],
        "mcp" => vec![
            ProvisionResponse::Selection(vec![]),
            ProvisionResponse::Selection(vec![0]),
        ],
        "skills" => vec![ProvisionResponse::Selection(vec![0])],
        "telegram" => vec![
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Selection(vec![0]),
        ],
        "discord" => vec![
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Selection(vec![0]),
        ],
        "slack" => vec![
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Text(String::new()),
        ],
        "signal" => vec![ProvisionResponse::Text(String::new())],
        "matrix" => vec![
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Text(String::new()),
        ],
        "mattermost" => vec![
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Text(String::new()),
        ],
        "imessage" => vec![ProvisionResponse::Text(String::new())],
        "lark" => vec![ProvisionResponse::Text(String::new())],
        "dingtalk" => vec![ProvisionResponse::Text(String::new())],
        "nextcloud-talk" => vec![ProvisionResponse::Text(String::new())],
        "qq" => vec![ProvisionResponse::Text(String::new())],
        "email" => vec![ProvisionResponse::Text(String::new())],
        "irc" => vec![ProvisionResponse::Text(String::new())],
        "linq" => vec![ProvisionResponse::Text(String::new())],
        "whatsapp-cloud" => vec![ProvisionResponse::Text(String::new())],
        "memory" => vec![
            ProvisionResponse::Selection(vec![0]),
            ProvisionResponse::Text(String::new()),
        ],
        "runtime" => vec![ProvisionResponse::Selection(vec![0])],
        "proxy" => vec![
            ProvisionResponse::Selection(vec![0]),
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Text(String::new()),
        ],
        "tunnel" => vec![ProvisionResponse::Selection(vec![0])],
        "gateway" => vec![ProvisionResponse::Selection(vec![0])],
        "browser" => vec![ProvisionResponse::Selection(vec![0])],
        "web-search" => vec![
            ProvisionResponse::Selection(vec![0]),
            ProvisionResponse::Text(String::new()),
        ],
        "composio" => vec![ProvisionResponse::Text(String::new())],
        "agents" => vec![
            ProvisionResponse::Selection(vec![]),
            ProvisionResponse::Selection(vec![0]),
        ],
        "model-routes" => vec![ProvisionResponse::Selection(vec![1])],
        "embedding-routes" => vec![ProvisionResponse::Selection(vec![1])],
        "secrets" => vec![ProvisionResponse::Selection(vec![0])],
        "multimodal" => vec![
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Text(String::new()),
            ProvisionResponse::Selection(vec![0]),
        ],
        "hardware" => vec![ProvisionResponse::Selection(vec![0])],
        #[cfg(feature = "whatsapp-web")]
        "whatsapp-web" => vec![ProvisionResponse::Text(String::new())],
        _ => vec![ProvisionResponse::Text(String::new())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_returns_none_for_unknown() {
        assert!(provisioner_for("nope").is_none());
    }

    #[test]
    fn persona_is_always_registered() {
        let p = provisioner_for("persona");
        assert!(p.is_some());
        assert_eq!(p.unwrap().name(), "persona");
    }

    #[test]
    fn available_includes_persona() {
        let all = available();
        assert!(all.iter().any(|(n, _)| *n == "persona"));
    }

    #[test]
    fn available_includes_all_core_channels() {
        let all = available();
        let names: Vec<_> = all.iter().map(|(n, _)| *n).collect();
        let expected = [
            "telegram",
            "discord",
            "slack",
            "signal",
            "matrix",
            "mattermost",
            "imessage",
            "lark",
            "dingtalk",
            "nextcloud-talk",
            "qq",
            "email",
            "irc",
            "linq",
            "whatsapp-cloud",
        ];
        for name in expected {
            assert!(names.contains(&name), "{name} should be registered");
        }
    }

    #[test]
    fn available_includes_runtime_surfaces() {
        let all = available();
        let names: Vec<_> = all.iter().map(|(n, _)| *n).collect();
        let expected = [
            "memory",
            "runtime",
            "proxy",
            "tunnel",
            "gateway",
            "browser",
            "web-search",
            "composio",
            "agents",
            "model-routes",
            "embedding-routes",
            "secrets",
            "multimodal",
            "hardware",
        ];
        for name in expected {
            assert!(names.contains(&name), "{name} should be registered");
        }
    }

    #[test]
    fn available_is_non_empty_when_whatsapp_web_enabled() {
        #[cfg(feature = "whatsapp-web")]
        {
            assert!(!available().is_empty());
        }
        #[cfg(not(feature = "whatsapp-web"))]
        {
            assert!(
                !available().is_empty(),
                "persona should always be available"
            );
        }
    }
}
