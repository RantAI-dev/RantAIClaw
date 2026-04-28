//! MCP startup check — verifies launch commands resolve to executables.

use async_trait::async_trait;

use crate::doctor::{CheckResult, DoctorCheck, DoctorContext};

pub struct McpStartupCheck;

#[async_trait]
impl DoctorCheck for McpStartupCheck {
    fn name(&self) -> &'static str { "mcp.startup" }
    fn category(&self) -> &'static str { "live" }
    async fn run(&self, ctx: &DoctorContext) -> CheckResult {
        let servers = &ctx.config.mcp_servers;
        if servers.is_empty() {
            return CheckResult::info(self.name(), "no MCP servers configured").with_category(self.category());
        }

        let mut missing: Vec<String> = Vec::new();
        let mut ok_count = 0usize;
        for (name, srv) in servers {
            if which::which(&srv.command).is_ok() { ok_count += 1; }
            else { missing.push(format!("{name} ({})", srv.command)); }
        }

        if missing.is_empty() {
            CheckResult::ok(self.name(), format!("{ok_count} MCP server(s) launchable")).with_category(self.category())
        } else {
            CheckResult::fail(self.name(), format!("commands not on PATH: {}", missing.join(", ")))
                .with_category(self.category())
                .with_hint("install the missing binaries or run: rantaiclaw setup mcp")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::profile::Profile;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn ctx(cfg: Config) -> (DoctorContext, TempDir) {
        let tmp = TempDir::new().unwrap();
        let p = Profile { name: "test".into(), root: tmp.path().to_path_buf() };
        (DoctorContext { profile: p, config: cfg, offline: false }, tmp)
    }

    #[tokio::test]
    async fn no_mcp_servers_returns_info() {
        let cfg = Config::default();
        let (c, _t) = ctx(cfg);
        let r = McpStartupCheck.run(&c).await;
        assert_eq!(r.severity, crate::doctor::Severity::Info);
    }

    #[tokio::test]
    async fn missing_command_returns_fail() {
        let mut cfg = Config::default();
        let mut servers = HashMap::new();
        servers.insert(
            "fake".to_string(),
            crate::config::schema::McpServerConfig {
                command: "definitely-not-a-real-binary-xyz123".into(),
                args: vec![],
                env: HashMap::new(),
            },
        );
        cfg.mcp_servers = servers;
        let (c, _t) = ctx(cfg);
        let r = McpStartupCheck.run(&c).await;
        assert_eq!(r.severity, crate::doctor::Severity::Fail);
        assert!(r.message.contains("definitely-not-a-real-binary-xyz123"));
    }
}
