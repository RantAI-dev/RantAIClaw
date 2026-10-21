use super::traits::{Tool, ToolResult};
use crate::skills::SkillTool;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;

use crate::security::SecurityPolicy;

/// Maximum execution time for a skill tool command.
const SKILL_TOOL_TIMEOUT_SECS: u64 = 120;
/// Maximum output size in bytes (1MB).
const MAX_OUTPUT_BYTES: usize = 1_048_576;

/// Adapter that wraps a skill's `[[tools]]` definition into a callable `Tool`.
///
/// This bridges the gap between skill tool descriptions (which are normally only
/// injected into the system prompt as text) and actual function-calling tools
/// that the LLM can invoke directly via the tool-use protocol.
pub struct SkillToolAdapter {
    /// Prefixed name: `skill_<skill_name>_<tool_name>` to avoid collisions.
    prefixed_name: String,
    /// Original skill tool definition.
    tool: SkillTool,
    /// Name of the parent skill (for logging/description).
    skill_name: String,
    /// Policy every shell skill's command is checked against before it runs.
    /// A skill's stored command runs without a shell, so the credential
    /// directory check reads its argv directly.
    security: Arc<SecurityPolicy>,
}

/// The name a skill tool's adapter gets: `skill_<skill>_<tool>`, with every
/// character that is neither alphanumeric nor `_` replaced by `_`. The
/// `skill_` prefix is what keeps a skill adapter from shadowing a built-in
/// tool of the same name.
pub(crate) fn skill_tool_name(skill_name: &str, tool_name: &str) -> String {
    format!(
        "skill_{}_{}",
        sanitize_name_part(skill_name),
        sanitize_name_part(tool_name)
    )
}

/// One part of an adapter name, with every character that is neither
/// alphanumeric nor `_` replaced by `_`.
fn sanitize_name_part(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl SkillToolAdapter {
    pub fn new(skill_name: &str, tool: SkillTool, security: Arc<SecurityPolicy>) -> Self {
        let prefixed_name = skill_tool_name(skill_name, &tool.name);
        Self {
            prefixed_name,
            tool,
            skill_name: skill_name.to_string(),
            security,
        }
    }
}

#[async_trait]
impl Tool for SkillToolAdapter {
    fn name(&self) -> &str {
        &self.prefixed_name
    }

    fn description(&self) -> &str {
        &self.tool.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        match self.tool.kind.as_str() {
            "shell" => json!({
                "type": "object",
                "properties": {
                    "args": {
                        "type": "string",
                        "description": "Additional arguments to pass to the command"
                    }
                },
                "required": []
            }),
            "http" => json!({
                "type": "object",
                "properties": {
                    "body": {
                        "type": "string",
                        "description": "Request body (JSON string)"
                    },
                    "method": {
                        "type": "string",
                        "description": "HTTP method (GET, POST, etc.)",
                        "default": "GET"
                    }
                },
                "required": []
            }),
            _ => json!({
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "Input for the tool"
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        match self.tool.kind.as_str() {
            "shell" => self.execute_shell(args).await,
            "http" => self.execute_http(args).await,
            _ => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unsupported skill tool kind: {}", self.tool.kind)),
            }),
        }
    }
}

impl SkillToolAdapter {
    async fn execute_shell(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Refuse a command that names a credential directory before anything
        // runs. The stored command is not passed through a shell, so `~` is
        // not expanded by the OS: the policy check expands it itself.
        // The model-supplied `args` is appended to the same argv, so it is checked too.
        let extra_args = args.get("args").and_then(|v| v.as_str()).unwrap_or("");
        let named = format!("{} {}", self.tool.command, extra_args);
        if let Some(dir) = self.security.command_names_credential_dir(&named) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(crate::security::policy::credential_refusal(&dir)),
            });
        }

        // Split the stored command into program + args to avoid sh -c injection.
        // The command field is typically "bun run /path/to/script.js" or "python3 /path".
        let parts: Vec<&str> = self.tool.command.split_whitespace().collect();
        let (program, base_args) = match parts.split_first() {
            Some((prog, rest)) => (*prog, rest.to_vec()),
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("Empty command in skill tool definition".to_string()),
                });
            }
        };

        let mut cmd = Command::new(program);
        for a in &base_args {
            cmd.arg(a);
        }

        // Append LLM-provided extra args as a single argument (not shell-expanded)
        if !extra_args.is_empty() {
            cmd.arg(extra_args);
        }

        let result =
            tokio::time::timeout(Duration::from_secs(SKILL_TOOL_TIMEOUT_SECS), cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                // Truncate if too large
                if stdout.len() > MAX_OUTPUT_BYTES {
                    stdout.truncate(MAX_OUTPUT_BYTES);
                    stdout.push_str("\n... [output truncated]");
                }

                if output.status.success() {
                    Ok(ToolResult {
                        success: true,
                        output: stdout,
                        error: if stderr.is_empty() {
                            None
                        } else {
                            Some(stderr)
                        },
                    })
                } else {
                    Ok(ToolResult {
                        success: false,
                        output: stdout,
                        error: Some(format!(
                            "Command exited with code {}: {}",
                            output.status.code().unwrap_or(-1),
                            stderr
                        )),
                    })
                }
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to execute command: {e}")),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Command timed out after {SKILL_TOOL_TIMEOUT_SECS}s"
                )),
            }),
        }
    }

    async fn execute_http(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Validate method is a known HTTP method to prevent injection
        let method = args.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
        let allowed_methods = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
        let safe_method = if allowed_methods.contains(&method.to_uppercase().as_str()) {
            method.to_uppercase()
        } else {
            "GET".to_string()
        };

        let body = args.get("body").and_then(|v| v.as_str()).unwrap_or("");

        let url = &self.tool.command;

        // Use curl with explicit args array (no shell) to prevent injection
        let mut cmd = Command::new("curl");
        cmd.arg("-sS").arg("-X").arg(&safe_method);

        if !body.is_empty() {
            cmd.arg("-H").arg("Content-Type: application/json");
            cmd.arg("-d").arg(body);
        }
        cmd.arg(url);

        let result =
            tokio::time::timeout(Duration::from_secs(SKILL_TOOL_TIMEOUT_SECS), cmd.output()).await;

        match result {
            Ok(Ok(output)) => {
                let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
                if stdout.len() > MAX_OUTPUT_BYTES {
                    stdout.truncate(MAX_OUTPUT_BYTES);
                    stdout.push_str("\n... [output truncated]");
                }
                Ok(ToolResult {
                    success: output.status.success(),
                    output: stdout,
                    error: if output.status.success() {
                        None
                    } else {
                        Some(String::from_utf8_lossy(&output.stderr).to_string())
                    },
                })
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("HTTP request failed: {e}")),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "HTTP request timed out after {SKILL_TOOL_TIMEOUT_SECS}s"
                )),
            }),
        }
    }
}

/// Create callable tool adapters from all loaded skills.
///
/// Returns a `Vec<Box<dyn Tool>>` that can be appended to the main tool registry.
///
/// Two skills can sanitise to the same adapter name (`my-skill` and `my.skill`
/// both give `skill_my_skill_greet`). The first one wins: a later skill never
/// replaces a tool the model already knows, and the warning names both skills
/// so the operator can see the collision.
pub fn skill_tools_from_skills(
    skills: &[crate::skills::Skill],
    security: &Arc<SecurityPolicy>,
) -> Vec<Box<dyn Tool>> {
    let mut tools: Vec<Box<dyn Tool>> = Vec::new();
    // The skill that owns each adapter name, so a collision can name both.
    let mut owners: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for skill in skills {
        for tool in &skill.tools {
            let name = skill_tool_name(&skill.name, &tool.name);
            if let Some(first_skill) = owners.get(&name) {
                tracing::warn!(
                    skill = %skill.name,
                    first_skill = %first_skill,
                    tool = %name,
                    "skipped: another skill already provides this tool name"
                );
                continue;
            }
            owners.insert(name, skill.name.clone());
            tools.push(Box::new(SkillToolAdapter::new(
                &skill.name,
                tool.clone(),
                Arc::clone(security),
            )));
        }
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy::default())
    }

    #[test]
    fn skill_tool_name_prefixed() {
        let st = SkillTool {
            name: "get_weather".to_string(),
            description: "Fetch forecast".to_string(),
            kind: "shell".to_string(),
            command: "curl wttr.in".to_string(),
            args: HashMap::new(),
        };
        let adapter = SkillToolAdapter::new("weather-skill", st, test_security());
        assert_eq!(adapter.name(), "skill_weather_skill_get_weather");
    }

    #[test]
    fn skill_tool_schema_shell() {
        let st = SkillTool {
            name: "run".to_string(),
            description: "Run something".to_string(),
            kind: "shell".to_string(),
            command: "echo hi".to_string(),
            args: HashMap::new(),
        };
        let adapter = SkillToolAdapter::new("test", st, test_security());
        let schema = adapter.parameters_schema();
        assert!(schema.get("properties").unwrap().get("args").is_some());
    }

    #[tokio::test]
    async fn skill_tool_execute_shell() {
        let st = SkillTool {
            name: "echo".to_string(),
            description: "Echo test".to_string(),
            kind: "shell".to_string(),
            command: "echo hello".to_string(),
            args: HashMap::new(),
        };
        let adapter = SkillToolAdapter::new("test", st, test_security());
        let result = adapter.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn skill_tool_execute_shell_refuses_a_credential_directory() {
        // A skill's stored command runs without a shell, so `~` is not expanded
        // by the OS. The policy check must still see the credential directory
        // in the argv and refuse before `Command::new` runs anything.
        let st = SkillTool {
            name: "peek".to_string(),
            description: "Read a key".to_string(),
            kind: "shell".to_string(),
            command: "cat ~/.ssh/id_rantaiclaw_fixture".to_string(),
            args: HashMap::new(),
        };
        let policy =
            Arc::new(SecurityPolicy::default().with_autonomy(crate::security::AutonomyLevel::Full));
        let adapter = SkillToolAdapter::new("test", st, policy);
        let result = adapter.execute(json!({})).await.unwrap();
        assert!(
            !result.success,
            "a skill naming a credential directory must be refused"
        );
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains("credential directory"),
            "the refusal must come from the policy, not from `cat` failing on a missing file: {error}"
        );
    }

    #[test]
    fn skill_tools_from_skills_creates_adapters() {
        let skills = vec![crate::skills::Skill {
            name: "weather".to_string(),
            description: "Get weather".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![
                SkillTool {
                    name: "forecast".to_string(),
                    description: "Get forecast".to_string(),
                    kind: "shell".to_string(),
                    command: "echo sunny".to_string(),
                    args: HashMap::new(),
                },
                SkillTool {
                    name: "current".to_string(),
                    description: "Get current".to_string(),
                    kind: "shell".to_string(),
                    command: "echo 25C".to_string(),
                    args: HashMap::new(),
                },
            ],
            prompts: vec![],
            location: None,
            requires: crate::skills::SkillRequires::default(),
            install_recipes: Vec::new(),
            remote: false,
            origin: None,
        }];
        let tools = skill_tools_from_skills(&skills, &test_security());
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name(), "skill_weather_forecast");
        assert_eq!(tools[1].name(), "skill_weather_current");
    }

    /// A skill with one `[[tools]]` entry.
    fn skill(name: &str, tool_name: &str) -> crate::skills::Skill {
        crate::skills::Skill {
            name: name.to_string(),
            description: "A skill".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![SkillTool {
                name: tool_name.to_string(),
                description: "Run something".to_string(),
                kind: "shell".to_string(),
                command: "echo hello".to_string(),
                args: HashMap::new(),
            }],
            prompts: vec![],
            location: None,
            requires: crate::skills::SkillRequires::default(),
            install_recipes: Vec::new(),
            remote: false,
            origin: None,
        }
    }

    /// The WARN lines emitted while `run` executes. A scoped subscriber, so
    /// parallel tests cannot see each other's events.
    fn warnings_from(run: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("lock the log buffer")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl tracing_subscriber::fmt::MakeWriter<'_> for Buffer {
            type Writer = Self;

            fn make_writer(&self) -> Self {
                self.clone()
            }
        }

        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = buffer.0.lock().expect("lock the log buffer").clone();
        String::from_utf8(bytes).expect("log output is utf-8")
    }

    /// Two skills can sanitise to the same adapter name. The first one wins:
    /// a later skill never replaces a tool the model already knows, and the
    /// warning names both skills so the operator can see the collision.
    #[test]
    fn two_skills_with_the_same_adapter_name_yield_one_tool_and_a_warning() {
        let skills = vec![skill("my-skill", "greet"), skill("my.skill", "greet")];
        let mut names = Vec::new();
        let logged = warnings_from(|| {
            let tools = skill_tools_from_skills(&skills, &test_security());
            names = tools.iter().map(|t| t.name().to_string()).collect();
        });

        assert_eq!(
            names,
            vec!["skill_my_skill_greet".to_string()],
            "the two skills did not collapse into one adapter"
        );
        assert!(
            logged.contains("my-skill") && logged.contains("my.skill"),
            "the warning must name both skills: {logged}"
        );
    }

    /// A supervised owner's first call to a shell-kind skill tool asks for
    /// approval. The skill tool's name is not on the auto-approve list, and
    /// that list matches names exactly.
    #[test]
    fn a_supervised_owner_approves_a_skill_tool_before_it_runs() {
        let manager = crate::approval::ApprovalManager::from_config(
            &crate::config::AutonomyConfig::default(),
        );
        assert!(
            manager.needs_approval("skill_greeter_greet"),
            "a skill tool ran without an approval prompt under the default config"
        );
        assert!(
            !manager.needs_approval("file_read"),
            "control: the auto-approved read tool still skips the prompt"
        );
    }

    /// A skill tool is not known to be read-only, so the read-only gate
    /// refuses it before its body runs.
    #[test]
    fn a_skill_tool_is_not_declared_read_only() {
        let adapter = SkillToolAdapter::new(
            "greeter",
            SkillTool {
                name: "greet".to_string(),
                description: "Greet".to_string(),
                kind: "shell".to_string(),
                command: "echo hello".to_string(),
                args: HashMap::new(),
            },
            test_security(),
        );
        assert!(
            !adapter.is_read_only_call(&json!({})),
            "a skill tool declared a read-only call, so read-only mode would run it"
        );
    }
}
