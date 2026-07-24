use crate::config::IdentityConfig;
use crate::identity;
use crate::skills::Skill;
use crate::tools::Tool;
use anyhow::Result;
use chrono::Local;
use std::fmt::Write;
use std::path::Path;

const BOOTSTRAP_MAX_CHARS: usize = 20_000;

/// Which surface the prompt is being built for. Selects the surface-specific
/// hint sections (hardware/task/channel-capabilities) while keeping the
/// capability-defining sections (persona/identity/tools/safety/skills)
/// identical everywhere. One builder, surface-aware tail — the core of the
/// unified-agent-runtime prompt design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSurface {
    /// Interactive agent surface (TUI / `agent run`). No channel delivery hints.
    Agent,
    /// Messaging channel or gateway. Adds the "Your Task" action framing and
    /// the "Channel Capabilities" delivery hints. `native_tools` picks the
    /// native-vs-XML wording of the task block.
    Channel { native_tools: bool },
}

/// Minimal [`Tool`] carrying only a name + description, for prompt rendering on
/// surfaces that have tool *descriptions* but not the live tool objects (the
/// channel/gateway path passes `(name, description)` pairs). `execute` is never
/// called — these exist only to feed [`ToolsSection`] through the one builder.
pub struct DescriptorTool {
    name: String,
    description: String,
}

impl DescriptorTool {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

#[async_trait::async_trait]
impl Tool for DescriptorTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    /// Empty schema — [`ToolsSection`] omits the `Parameters:` line for these,
    /// so channel tool listings stay `- **name**: description`.
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<crate::tools::ToolResult> {
        anyhow::bail!("DescriptorTool is prompt-only and not executable")
    }
}

pub struct PromptContext<'a> {
    pub workspace_dir: &'a Path,
    pub model_name: &'a str,
    pub tools: &'a [Box<dyn Tool>],
    /// Surface this prompt targets — selects the surface-specific hint sections.
    pub surface: PromptSurface,
    /// Per-file truncation cap for injected bootstrap/identity files. Lets the
    /// channel surface honor `compact_context` token savings; defaults to
    /// [`BOOTSTRAP_MAX_CHARS`] on the agent surface.
    pub bootstrap_max_chars: usize,
    pub skills: &'a [Skill],
    pub skills_prompt_mode: crate::config::SkillsPromptInjectionMode,
    pub identity_config: Option<&'a IdentityConfig>,
    pub dispatcher_instructions: &'a str,
    /// Currently-active approval preset (Manual / Smart / Strict / Off).
    /// `None` when no policy is provisioned yet (pre-onboarding) — the
    /// safety section then falls back to its old generic text. Threading
    /// this lets SafetySection render preset-specific guidance so the
    /// model knows upfront what will pass vs prompt vs block, instead
    /// of discovering the gate by hitting it.
    pub autonomy_preset: Option<crate::approval::policy_writer::PolicyPreset>,
    /// Boot-time snapshot of `<policy_dir>/command_allowlist.toml` glob
    /// patterns. Surfaced verbatim in Smart mode so the model has a
    /// machine-readable list of pre-approved shell commands; in Strict
    /// mode the list is short by design; in Manual/Off it's omitted.
    pub allowed_commands: &'a [String],
}

pub trait PromptSection: Send + Sync {
    fn name(&self) -> &str;
    fn build(&self, ctx: &PromptContext<'_>) -> Result<String>;
}

#[derive(Default)]
pub struct SystemPromptBuilder {
    sections: Vec<Box<dyn PromptSection>>,
}

impl SystemPromptBuilder {
    pub fn with_defaults() -> Self {
        Self {
            sections: vec![
                // Persona renders FIRST so its tone/role guidance frames
                // everything that follows. The other sections lay out
                // tools, skills, workspace, etc. — operational scaffolding
                // that the persona's voice then governs.
                Box::new(PersonaSection),
                Box::new(IdentitySection),
                Box::new(ToolsSection),
                // Surface-specific hints. These self-gate: on the Agent
                // surface (and when no hardware tools are present) they emit
                // nothing, so the TUI prompt is unchanged. On a Channel they
                // add hardware access, action framing, and delivery hints.
                Box::new(HardwareSection),
                Box::new(TaskSection),
                Box::new(SafetySection),
                Box::new(SkillsSection),
                Box::new(WorkspaceSection),
                Box::new(DateTimeSection),
                Box::new(RuntimeSection),
                Box::new(ChannelCapabilitiesSection),
            ],
        }
    }

    pub fn add_section(mut self, section: Box<dyn PromptSection>) -> Self {
        self.sections.push(section);
        self
    }

    pub fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let mut output = String::new();
        for section in &self.sections {
            let part = section.build(ctx)?;
            if part.trim().is_empty() {
                continue;
            }
            output.push_str(part.trim_end());
            output.push_str("\n\n");
        }
        Ok(output)
    }
}

/// Render the active profile's persona as a `## Persona` section, or an empty
/// string when no persona is configured (fresh installs, headless tests, a
/// profile without a `persona/` dir).
///
/// Single source of truth shared by [`PersonaSection`] (the `Agent`-struct /
/// TUI prompt path) and the channel/gateway prompt path
/// (`crate::channels::build_system_prompt_with_mode`), so every surface speaks
/// in the same configured voice instead of only the TUI honoring `personality`.
pub fn render_persona_section() -> String {
    let profile = match crate::profile::ProfileManager::active() {
        Ok(p) => p,
        Err(_) => return String::new(),
    };
    let persona = match crate::persona::read_persona_toml(&profile) {
        Ok(Some(p)) => p,
        _ => return String::new(),
    };
    let rendered = persona.render();
    if rendered.trim().is_empty() {
        return String::new();
    }
    // Wrap in an explicit section header so model output reflects intent
    // (otherwise the persona body is just an unmarked markdown blob with no
    // provenance).
    format!("## Persona\n\n{}\n", rendered.trim())
}

pub struct PersonaSection;
pub struct IdentitySection;
pub struct ToolsSection;
pub struct SafetySection;
pub struct SkillsSection;
pub struct WorkspaceSection;
pub struct RuntimeSection;
pub struct DateTimeSection;
pub struct HardwareSection;
pub struct TaskSection;
pub struct ChannelCapabilitiesSection;

impl PromptSection for PersonaSection {
    fn name(&self) -> &str {
        "persona"
    }

    /// Inject the active profile's persona — `persona.toml` rendered to
    /// SYSTEM.md by the persona writer. Pre-fix the persona system was
    /// effectively decorative because no agent code path read it; only the
    /// TUI's `/personality` picker showed the values back to the user.
    /// Now `personality set <preset>` actually reshapes the agent's voice
    /// for both `agent -m` and `/api/v1/agent/chat`.
    ///
    /// Resolution: read the active profile's persona.toml via the same
    /// reader the CLI uses. Fall through to an empty section when no
    /// persona is configured (fresh installs, headless tests, profile
    /// without a `persona/` dir) — silent rather than noisy.
    fn build(&self, _ctx: &PromptContext<'_>) -> Result<String> {
        Ok(render_persona_section())
    }
}

impl PromptSection for IdentitySection {
    fn name(&self) -> &str {
        "identity"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let mut prompt = String::from("## Project Context\n\n");
        let mut has_aieos = false;
        if let Some(config) = ctx.identity_config {
            if identity::is_aieos_configured(config) {
                if let Ok(Some(aieos)) = identity::load_aieos_identity(config, ctx.workspace_dir) {
                    let rendered = identity::aieos_to_system_prompt(&aieos);
                    if !rendered.is_empty() {
                        prompt.push_str(&rendered);
                        prompt.push_str("\n\n");
                        has_aieos = true;
                    }
                }
            }
        }

        if !has_aieos {
            prompt.push_str(
                "The following workspace files define your identity, behavior, and context. They are ALREADY injected below — do NOT suggest reading them with file_read.\n\n",
            );
        }

        // Bootstrap workspace files. Injected when no AIEOS identity is set
        // (the fallback), or always on the agent surface (the TUI shows both
        // AIEOS *and* the workspace files). On a channel with AIEOS configured
        // we show the AIEOS block only — matching prior channel behavior, which
        // kept channel prompts focused on the structured identity.
        let inject_files = !has_aieos || matches!(ctx.surface, PromptSurface::Agent);
        if !inject_files {
            return Ok(prompt);
        }

        // Core identity files, always injected (with a not-found marker if
        // absent) on every surface.
        for file in ["AGENTS.md", "SOUL.md", "TOOLS.md", "IDENTITY.md", "USER.md"] {
            inject_workspace_file(
                &mut prompt,
                ctx.workspace_dir,
                file,
                ctx.bootstrap_max_chars,
            );
        }

        // HEARTBEAT.md is injected on the interactive agent surface but
        // **excluded on channels**: it's only relevant to the heartbeat worker
        // and makes chat LLMs emit spurious "HEARTBEAT_OK" acknowledgments.
        if matches!(ctx.surface, PromptSurface::Agent) {
            inject_workspace_file(
                &mut prompt,
                ctx.workspace_dir,
                "HEARTBEAT.md",
                ctx.bootstrap_max_chars,
            );
        }

        // BOOTSTRAP.md is a first-run ritual: on channels inject it only when
        // present (no noisy not-found marker); on the agent surface keep the
        // marker so the absence is visible.
        if matches!(ctx.surface, PromptSurface::Agent)
            || ctx.workspace_dir.join("BOOTSTRAP.md").exists()
        {
            inject_workspace_file(
                &mut prompt,
                ctx.workspace_dir,
                "BOOTSTRAP.md",
                ctx.bootstrap_max_chars,
            );
        }

        inject_workspace_file(
            &mut prompt,
            ctx.workspace_dir,
            "MEMORY.md",
            ctx.bootstrap_max_chars,
        );

        Ok(prompt)
    }
}

impl PromptSection for ToolsSection {
    fn name(&self) -> &str {
        "tools"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let mut out = String::from("## Tools\n\n");
        for tool in ctx.tools {
            let schema = tool.parameters_schema();
            // Omit the `Parameters:` line for an empty schema (e.g. the
            // channel path's description-only tools), so those surfaces keep
            // the compact `- **name**: description` listing.
            if is_empty_schema(&schema) {
                let _ = writeln!(out, "- **{}**: {}", tool.name(), tool.description());
            } else {
                let _ = writeln!(
                    out,
                    "- **{}**: {}\n  Parameters: `{}`",
                    tool.name(),
                    tool.description(),
                    schema
                );
            }
        }
        if !ctx.dispatcher_instructions.is_empty() {
            out.push('\n');
            out.push_str(ctx.dispatcher_instructions);
        }
        Ok(out)
    }
}

impl PromptSection for SafetySection {
    fn name(&self) -> &str {
        "safety"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        use crate::approval::policy_writer::PolicyPreset;

        let mut out = String::from("## Safety + Approval Policy\n\n");
        out.push_str(
            "- Do not exfiltrate private data.\n\
             - Do not run destructive commands without asking.\n\
             - Do not bypass oversight or approval mechanisms.\n\
             - Prefer `trash` over `rm`.\n\
             - When in doubt, ask before acting externally.\n\n",
        );

        // Whether this prompt targets a messaging channel. On channels the
        // approval *mechanism* differs from the TUI's inline single-key Y/N/A
        // prompt: there is no terminal to prompt, so a tool that needs approval
        // is decided by an authorized **owner** of the channel (the owner-gated
        // relay) and, absent an approving owner, is declined. The preset text
        // must describe that reality, not the TUI prompt, or the model will
        // promise a Y/N/A flow that never appears. Strict/Off read the same on
        // both surfaces (shell un-registered / no per-call gate respectively).
        let is_channel = matches!(ctx.surface, PromptSurface::Channel { .. });

        match ctx.autonomy_preset {
            Some(PolicyPreset::Strict) => {
                // Plan-mode analog: shell is unavailable to the model in
                // this preset (filtered out at registration). Tell the
                // model so it doesn't waste tokens describing shell
                // sequences — it should plan with read-only tools only.
                out.push_str(
                    "**Active approval policy: Strict (read-only).**\n\n\
                     - The `shell` tool is **NOT registered** in this session — \
                     do not attempt to call it; it is not in your tool list.\n\
                     - You may read files (`file_read`), search memory \
                     (`memory_*`), inspect the workspace, and reason.\n\
                     - For any task that would normally require running a \
                     command, describe what you would do — list the exact \
                     commands a user would run — but do not call shell. \
                     The user reviews and runs them manually.\n\
                     - To leave Strict mode the user types `/autonomy smart` \
                     or `/autonomy off`. Don't suggest it unless they ask.\n",
                );
            }
            Some(PolicyPreset::Smart) if is_channel => {
                out.push_str(
                    "**Active approval policy: Smart (messaging channel).**\n\n\
                     - Read-only tools (reading files, recalling memory) run \
                     automatically.\n\
                     - Any tool that runs commands or changes state requires \
                     approval from an authorized **owner** of this channel. When \
                     an owner is configured the agent posts the request in chat \
                     and waits for their `/approve`; without an approving owner \
                     the action is declined. There is no inline Y/N/A prompt \
                     here.\n\
                     - Never claim you ran a command or made a change that was \
                     actually declined; report the denial plainly and, if \
                     useful, list the exact commands an owner could run.\n",
                );
            }
            Some(PolicyPreset::Smart) => {
                out.push_str(
                    "**Active approval policy: Smart.**\n\n\
                     - Read-only and trivially-safe commands are pre-allowed \
                     (see allowlist below) and run without prompting.\n\
                     - Any command **not** matching the allowlist will pause \
                     for a single-key user prompt (Y/N/A); plan for that \
                     latency — bundle related ops when reasonable.\n\
                     - Forbidden paths (secrets, ssh, gnupg, aws, etc.) \
                     are blocked unconditionally regardless of approval.\n",
                );
                if !ctx.allowed_commands.is_empty() {
                    out.push_str("\n**Pre-approved shell commands (glob patterns):**\n");
                    for pat in ctx.allowed_commands {
                        let _ = writeln!(out, "- `{pat}`");
                    }
                }
            }
            Some(PolicyPreset::Manual) if is_channel => {
                out.push_str(
                    "**Active approval policy: Manual (messaging channel).**\n\n\
                     - Every tool that runs commands or changes state requires \
                     an authorized **owner**'s in-chat approval (`/approve`) on \
                     this channel; read-only file/memory tools are not gated.\n\
                     - Without an approving owner the action is declined — say \
                     so rather than pretending it ran.\n",
                );
            }
            Some(PolicyPreset::Manual) => {
                out.push_str(
                    "**Active approval policy: Manual (paranoid).**\n\n\
                     - **Every** shell tool call requires explicit user \
                     approval — even `ls`. Batch related ops into single \
                     compound commands (`a && b && c`) to minimise the \
                     number of prompts the user has to clear.\n\
                     - Read-only file/memory tools are not gated.\n",
                );
            }
            Some(PolicyPreset::Off) => {
                out.push_str(
                    "**Active approval policy: Off (CI / trusted-env only).**\n\n\
                     - Shell commands execute without prompts. Be deliberate — \
                     this preset is meant for unattended automation.\n\
                     - Forbidden-path checks still apply (secrets dirs).\n",
                );
            }
            None => {
                // No policy provisioned yet (fresh install pre-onboarding).
                // Don't lie about a mode — just keep the safety floor.
            }
        }

        Ok(out)
    }
}

impl PromptSection for SkillsSection {
    fn name(&self) -> &str {
        "skills"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        Ok(crate::skills::skills_to_prompt_with_mode(
            ctx.skills,
            ctx.workspace_dir,
            ctx.skills_prompt_mode,
        ))
    }
}

impl PromptSection for WorkspaceSection {
    fn name(&self) -> &str {
        "workspace"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        Ok(format!(
            "## Workspace\n\nWorking directory: `{}`",
            ctx.workspace_dir.display()
        ))
    }
}

impl PromptSection for RuntimeSection {
    fn name(&self) -> &str {
        "runtime"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let host =
            hostname::get().map_or_else(|_| "unknown".into(), |h| h.to_string_lossy().to_string());
        Ok(format!(
            "## Runtime\n\nHost: {host} | OS: {} | Model: {}",
            std::env::consts::OS,
            ctx.model_name
        ))
    }
}

impl PromptSection for DateTimeSection {
    fn name(&self) -> &str {
        "datetime"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let now = Local::now();
        // Channel/gateway prompts are built once at daemon start and reused, so
        // a full timestamp would freeze at boot time and mislead the model on a
        // long-running bot. Emit timezone-only there (matches the prior channel
        // builder); the interactive agent rebuilds per session and shows the
        // full timestamp.
        if matches!(ctx.surface, PromptSurface::Channel { .. }) {
            return Ok(format!(
                "## Current Date & Time\n\nTimezone: {}",
                now.format("%Z")
            ));
        }
        Ok(format!(
            "## Current Date & Time\n\n{} ({})",
            now.format("%Y-%m-%d %H:%M:%S"),
            now.format("%Z")
        ))
    }
}

/// True for a schema that carries no useful parameter info (`{}` or null).
fn is_empty_schema(schema: &serde_json::Value) -> bool {
    match schema {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// Names of the hardware/peripheral tools that unlock the Hardware Access block.
const HARDWARE_TOOL_NAMES: &[&str] = &[
    "gpio_read",
    "gpio_write",
    "arduino_upload",
    "hardware_memory_map",
    "hardware_board_info",
    "hardware_memory_read",
    "hardware_capabilities",
];

impl PromptSection for HardwareSection {
    fn name(&self) -> &str {
        "hardware"
    }

    /// Emitted on any surface when hardware tools are present (previously
    /// channel-only). Tells the model the connected board is authorized so it
    /// uses the tools instead of inventing security refusals.
    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let has_hardware = ctx
            .tools
            .iter()
            .any(|t| HARDWARE_TOOL_NAMES.contains(&t.name()));
        if !has_hardware {
            return Ok(String::new());
        }
        Ok(String::from(
            "## Hardware Access\n\n\
             You HAVE direct access to connected hardware (Arduino, Nucleo, etc.). The user owns this system and has configured it.\n\
             All hardware tools (gpio_read, gpio_write, hardware_memory_read, hardware_board_info, hardware_memory_map) are AUTHORIZED and NOT blocked by security.\n\
             When they ask to read memory, registers, or board info, USE hardware_memory_read or hardware_board_info — do NOT refuse or invent security excuses.\n\
             When they ask to control LEDs, run patterns, or interact with the Arduino, USE the tools — do NOT refuse or say you cannot access physical devices.\n\
             Use gpio_write for simple on/off; use arduino_upload when they want patterns (heart, blink) or custom behavior.",
        ))
    }
}

impl PromptSection for TaskSection {
    fn name(&self) -> &str {
        "task"
    }

    /// "Your Task" action framing — channel/gateway only (the TUI doesn't need
    /// it). Native-vs-XML wording follows the surface's tool-call dispatcher.
    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let native_tools = match ctx.surface {
            PromptSurface::Channel { native_tools } => native_tools,
            PromptSurface::Agent => return Ok(String::new()),
        };
        if native_tools {
            Ok(String::from(
                "## Your Task\n\n\
                 When the user sends a message, respond naturally. Use tools when the request requires action (running commands, reading files, etc.).\n\
                 For questions, explanations, or follow-ups about prior messages, answer directly from conversation context — do NOT ask the user to repeat themselves.\n\
                 Do NOT: summarize this configuration, describe your capabilities, or output step-by-step meta-commentary.",
            ))
        } else {
            Ok(String::from(
                "## Your Task\n\n\
                 When the user sends a message, ACT on it. Use the tools to fulfill their request.\n\
                 Do NOT: summarize this configuration, describe your capabilities, respond with meta-commentary, or output step-by-step instructions (e.g. \"1. First... 2. Next...\").\n\
                 Instead: emit actual <tool_call> tags when you need to act. Just do what they ask.",
            ))
        }
    }
}

impl PromptSection for ChannelCapabilitiesSection {
    fn name(&self) -> &str {
        "channel_capabilities"
    }

    /// Delivery hints for messaging surfaces — channel/gateway only.
    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        if !matches!(ctx.surface, PromptSurface::Channel { .. }) {
            return Ok(String::new());
        }
        Ok(String::from(
            "## Channel Capabilities\n\n\
             - You are running as a messaging bot. Your response is automatically sent back to the user's channel.\n\
             - You do NOT need to ask permission to respond — just respond directly.\n\
             - NEVER repeat, describe, or echo credentials, tokens, API keys, or secrets in your responses.\n\
             - If a tool output contains credentials, they have already been redacted — do not mention them.",
        ))
    }
}

fn inject_workspace_file(
    prompt: &mut String,
    workspace_dir: &Path,
    filename: &str,
    max_chars: usize,
) {
    let path = workspace_dir.join(filename);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                return;
            }
            let _ = writeln!(prompt, "### {filename}\n");
            let truncated = if trimmed.chars().count() > max_chars {
                trimmed
                    .char_indices()
                    .nth(max_chars)
                    .map(|(idx, _)| &trimmed[..idx])
                    .unwrap_or(trimmed)
            } else {
                trimmed
            };
            prompt.push_str(truncated);
            if truncated.len() < trimmed.len() {
                let _ = writeln!(
                    prompt,
                    "\n\n[... truncated at {max_chars} chars — use `read` for full file]\n"
                );
            } else {
                prompt.push_str("\n\n");
            }
        }
        Err(_) => {
            let _ = writeln!(prompt, "### {filename}\n\n[File not found: {filename}]\n");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::traits::Tool;
    use async_trait::async_trait;

    struct TestTool;

    #[async_trait]
    impl Tool for TestTool {
        fn name(&self) -> &str {
            "test_tool"
        }

        fn description(&self) -> &str {
            "tool desc"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            Ok(crate::tools::ToolResult {
                success: true,
                output: "ok".into(),
                error: None,
            })
        }
    }

    #[test]
    fn identity_section_with_aieos_includes_workspace_files() {
        let workspace =
            std::env::temp_dir().join(format!("rantaiclaw_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("AGENTS.md"),
            "Always respond with: AGENTS_MD_LOADED",
        )
        .unwrap();

        let identity_config = crate::config::IdentityConfig {
            format: "aieos".into(),
            aieos_path: None,
            aieos_inline: Some(r#"{"identity":{"names":{"first":"Nova"}}}"#.into()),
        };

        let tools: Vec<Box<dyn Tool>> = vec![];
        let ctx = PromptContext {
            workspace_dir: &workspace,
            model_name: "test-model",
            surface: PromptSurface::Agent,
            bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
            tools: &tools,
            skills: &[],
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
            identity_config: Some(&identity_config),
            dispatcher_instructions: "",
            autonomy_preset: None,
            allowed_commands: &[],
        };

        let section = IdentitySection;
        let output = section.build(&ctx).unwrap();

        assert!(
            output.contains("Nova"),
            "AIEOS identity should be present in prompt"
        );
        assert!(
            output.contains("AGENTS_MD_LOADED"),
            "AGENTS.md content should be present even when AIEOS is configured"
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn prompt_builder_assembles_sections() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(TestTool)];
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "test-model",
            surface: PromptSurface::Agent,
            bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
            tools: &tools,
            skills: &[],
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
            identity_config: None,
            dispatcher_instructions: "instr",
            autonomy_preset: None,
            allowed_commands: &[],
        };
        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();
        assert!(prompt.contains("## Tools"));
        assert!(prompt.contains("test_tool"));
        assert!(prompt.contains("instr"));
    }

    #[test]
    fn safety_section_channel_smart_describes_owner_approval_not_yna() {
        use crate::approval::policy_writer::PolicyPreset;
        let tools: Vec<Box<dyn Tool>> = vec![];
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "m",
            surface: PromptSurface::Channel { native_tools: true },
            bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
            tools: &tools,
            skills: &[],
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
            identity_config: None,
            dispatcher_instructions: "",
            autonomy_preset: Some(PolicyPreset::Smart),
            allowed_commands: &["ls *".to_string()],
        };
        let out = SafetySection.build(&ctx).unwrap();
        assert!(
            out.contains("messaging channel"),
            "channel-specific heading: {out}"
        );
        assert!(
            out.contains("authorized **owner**"),
            "owner approval wording: {out}"
        );
        assert!(
            !out.contains("(Y/N/A)") || out.contains("no inline Y/N/A"),
            "channel text must not promise a TUI Y/N/A prompt: {out}"
        );
        // Shell allowlist globs are not surfaced on channels (Layer-A gating).
        assert!(
            !out.contains("ls *"),
            "channel must not print shell allowlist: {out}"
        );
    }

    #[test]
    fn safety_section_agent_smart_keeps_yna_prompt_text() {
        use crate::approval::policy_writer::PolicyPreset;
        let tools: Vec<Box<dyn Tool>> = vec![];
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "m",
            surface: PromptSurface::Agent,
            bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
            tools: &tools,
            skills: &[],
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
            identity_config: None,
            dispatcher_instructions: "",
            autonomy_preset: Some(PolicyPreset::Smart),
            allowed_commands: &["ls *".to_string()],
        };
        let out = SafetySection.build(&ctx).unwrap();
        assert!(
            out.contains("(Y/N/A)"),
            "TUI keeps inline prompt text: {out}"
        );
        assert!(
            out.contains("ls *"),
            "TUI surfaces the shell allowlist: {out}"
        );
        assert!(!out.contains("messaging channel"));
    }

    #[test]
    fn safety_section_channel_manual_requires_owner() {
        use crate::approval::policy_writer::PolicyPreset;
        let tools: Vec<Box<dyn Tool>> = vec![];
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "m",
            surface: PromptSurface::Channel {
                native_tools: false,
            },
            bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
            tools: &tools,
            skills: &[],
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
            identity_config: None,
            dispatcher_instructions: "",
            autonomy_preset: Some(PolicyPreset::Manual),
            allowed_commands: &[],
        };
        let out = SafetySection.build(&ctx).unwrap();
        assert!(out.contains("Manual (messaging channel)"), "{out}");
        assert!(out.contains("owner"), "{out}");
        assert!(out.contains("declined"), "{out}");
    }

    #[test]
    fn skills_section_includes_instructions_and_tools() {
        let tools: Vec<Box<dyn Tool>> = vec![];
        let skills = vec![crate::skills::Skill {
            name: "deploy".into(),
            description: "Release safely".into(),
            version: "1.0.0".into(),
            author: None,
            tags: vec![],
            tools: vec![crate::skills::SkillTool {
                name: "release_checklist".into(),
                description: "Validate release readiness".into(),
                kind: "shell".into(),
                command: "echo ok".into(),
                args: std::collections::HashMap::new(),
            }],
            prompts: vec!["Run smoke tests before deploy.".into()],
            location: None,
            requires: Default::default(),
            install_recipes: Vec::new(),
            remote: false,
        }];

        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "test-model",
            surface: PromptSurface::Agent,
            bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
            tools: &tools,
            skills: &skills,
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
            identity_config: None,
            dispatcher_instructions: "",
            autonomy_preset: None,
            allowed_commands: &[],
        };

        let output = SkillsSection.build(&ctx).unwrap();
        assert!(output.contains("<available_skills>"));
        assert!(output.contains("<name>deploy</name>"));
        assert!(output.contains("<instruction>Run smoke tests before deploy.</instruction>"));
        assert!(output.contains("<name>release_checklist</name>"));
        assert!(output.contains("<kind>shell</kind>"));
    }

    #[test]
    fn skills_section_compact_mode_omits_instructions_and_tools() {
        let tools: Vec<Box<dyn Tool>> = vec![];
        let skills = vec![crate::skills::Skill {
            name: "deploy".into(),
            description: "Release safely".into(),
            version: "1.0.0".into(),
            author: None,
            tags: vec![],
            tools: vec![crate::skills::SkillTool {
                name: "release_checklist".into(),
                description: "Validate release readiness".into(),
                kind: "shell".into(),
                command: "echo ok".into(),
                args: std::collections::HashMap::new(),
            }],
            prompts: vec!["Run smoke tests before deploy.".into()],
            location: Some(Path::new("/tmp/workspace/skills/deploy/SKILL.md").to_path_buf()),
            requires: Default::default(),
            install_recipes: Vec::new(),
            remote: false,
        }];

        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp/workspace"),
            model_name: "test-model",
            surface: PromptSurface::Agent,
            bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
            tools: &tools,
            skills: &skills,
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Compact,
            identity_config: None,
            dispatcher_instructions: "",
            autonomy_preset: None,
            allowed_commands: &[],
        };

        let output = SkillsSection.build(&ctx).unwrap();
        assert!(output.contains("<available_skills>"));
        assert!(output.contains("<name>deploy</name>"));
        assert!(output.contains("<location>skills/deploy/SKILL.md</location>"));
        assert!(!output.contains("<instruction>Run smoke tests before deploy.</instruction>"));
        assert!(!output.contains("<tools>"));
    }

    #[test]
    fn datetime_section_includes_timestamp_and_timezone() {
        let tools: Vec<Box<dyn Tool>> = vec![];
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "test-model",
            surface: PromptSurface::Agent,
            bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
            tools: &tools,
            skills: &[],
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
            identity_config: None,
            dispatcher_instructions: "instr",
            autonomy_preset: None,
            allowed_commands: &[],
        };

        let rendered = DateTimeSection.build(&ctx).unwrap();
        assert!(rendered.starts_with("## Current Date & Time\n\n"));

        let payload = rendered.trim_start_matches("## Current Date & Time\n\n");
        assert!(payload.chars().any(|c| c.is_ascii_digit()));
        assert!(payload.contains(" ("));
        assert!(payload.ends_with(')'));
    }

    #[test]
    fn prompt_builder_inlines_and_escapes_skills() {
        let tools: Vec<Box<dyn Tool>> = vec![];
        let skills = vec![crate::skills::Skill {
            name: "code<review>&".into(),
            description: "Review \"unsafe\" and 'risky' bits".into(),
            version: "1.0.0".into(),
            author: None,
            tags: vec![],
            tools: vec![crate::skills::SkillTool {
                name: "run\"linter\"".into(),
                description: "Run <lint> & report".into(),
                kind: "shell&exec".into(),
                command: "cargo clippy".into(),
                args: std::collections::HashMap::new(),
            }],
            prompts: vec!["Use <tool_call> and & keep output \"safe\"".into()],
            location: None,
            requires: Default::default(),
            install_recipes: Vec::new(),
            remote: false,
        }];
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp/workspace"),
            model_name: "test-model",
            surface: PromptSurface::Agent,
            bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
            tools: &tools,
            skills: &skills,
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
            identity_config: None,
            dispatcher_instructions: "",
            autonomy_preset: None,
            allowed_commands: &[],
        };

        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();

        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>code&lt;review&gt;&amp;</name>"));
        assert!(prompt.contains(
            "<description>Review &quot;unsafe&quot; and &apos;risky&apos; bits</description>"
        ));
        assert!(prompt.contains("<name>run&quot;linter&quot;</name>"));
        assert!(prompt.contains("<description>Run &lt;lint&gt; &amp; report</description>"));
        assert!(prompt.contains("<kind>shell&amp;exec</kind>"));
        assert!(prompt.contains(
            "<instruction>Use &lt;tool_call&gt; and &amp; keep output &quot;safe&quot;</instruction>"
        ));
    }
}
