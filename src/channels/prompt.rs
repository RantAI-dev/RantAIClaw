//! System-prompt assembly for channel turns.
//!
//! Moved out of `mod.rs` verbatim (plan 121, row 3). No behaviour change. The
//! two `build_system_prompt*` entry points are part of the module's external
//! surface and are re-exported from `mod.rs` so their public path is unchanged.

use super::BOOTSTRAP_MAX_CHARS;

/// Appended to a channel system prompt when the sender is an approval owner
/// (`can_approve` is true for them). Without it, a cautious model self-refuses
/// owner-only tools (e.g. `manage_permissions`, `issue_pairing_code`) because
/// their descriptions say "owner-only" and nothing tells the model the sender
/// IS an owner — even though the runtime has already authorized them. This does
/// NOT widen any permission: the runtime gate stays the sole enforcer; it only
/// stops the model from falsely declining an already-authorized request.
const CHANNEL_OWNER_CONTEXT: &str = "The person you are talking to is a verified OWNER of this bot: \
the runtime has already authorized them for owner-privileged actions. When they ask you to use an \
owner-only tool (for example manage_permissions or issue_pairing_code), use it on their behalf — do \
NOT refuse on the grounds that the tool is owner-only.";

/// Announce-capable channels — the set `deliver_if_configured`
/// (`src/cron/scheduler.rs`) can push a scheduled agent job's output to. Keep in
/// sync with that match.
pub fn channel_supports_announce_delivery(channel_name: &str) -> bool {
    matches!(
        channel_name,
        "telegram" | "discord" | "slack" | "mattermost"
    )
}

/// Guidance so the agent, when the user asks for a scheduled/recurring message or
/// reminder, creates a `cron_add` agent job whose `delivery` routes the output
/// back to THIS chat — and nowhere else. Only emitted for channels the scheduler
/// can actually deliver to.
pub(crate) fn channel_cron_delivery_instructions(
    channel_name: &str,
    reply_target: &str,
) -> Option<String> {
    if !channel_supports_announce_delivery(channel_name) {
        return None;
    }
    Some(format!(
        "You are talking to this user on the '{channel_name}' channel (their delivery \
address is '{reply_target}'). When they ask you to send them a message, reminder, or \
report on a schedule (e.g. \"message me every morning\"), create it with the cron_add \
tool as an agent job and set delivery to route the output back to THEM here: \
delivery = {{ \"mode\": \"announce\", \"channel\": \"{channel_name}\", \"to\": \"{reply_target}\" }}. \
The scheduled output is delivered only to this chat — it does not appear anywhere else. \
Do not ask the user for their chat id; use the address above."
    ))
}

pub(crate) fn build_channel_system_prompt(
    base_prompt: &str,
    channel_name: &str,
    reply_target: &str,
    is_owner: bool,
    delivery_instructions: Option<&str>,
) -> String {
    let mut prompt = if let Some(instructions) = delivery_instructions {
        if base_prompt.is_empty() {
            instructions.to_string()
        } else {
            format!("{base_prompt}\n\n{instructions}")
        }
    } else {
        base_prompt.to_string()
    };

    if let Some(cron) = channel_cron_delivery_instructions(channel_name, reply_target) {
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(&cron);
    }

    if is_owner {
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str(CHANNEL_OWNER_CONTEXT);
    }

    prompt
}

/// Load workspace identity files and build a system prompt.
///
/// Follows the `OpenClaw` framework structure by default:
/// 1. Tooling — tool list + descriptions
/// 2. Safety — guardrail reminder
/// 3. Skills — full skill instructions and tool metadata
/// 4. Workspace — working directory
/// 5. Bootstrap files — AGENTS, SOUL, TOOLS, IDENTITY, USER, BOOTSTRAP, MEMORY
/// 6. Date & Time — timezone for cache stability
/// 7. Runtime — host, OS, model
///
/// When `identity_config` is set to AIEOS format, the bootstrap files section
/// is replaced with the AIEOS identity data loaded from file or inline JSON.
///
/// Daily memory files (`memory/*.md`) are NOT injected — they are accessed
/// on-demand via `memory_recall` / `memory_search` tools.
pub fn build_system_prompt(
    workspace_dir: &std::path::Path,
    model_name: &str,
    tools: &[(&str, &str)],
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
) -> String {
    build_system_prompt_with_mode(
        workspace_dir,
        model_name,
        tools,
        skills,
        identity_config,
        bootstrap_max_chars,
        false,
        crate::config::SkillsPromptInjectionMode::Full,
    )
}

pub fn build_system_prompt_with_mode(
    workspace_dir: &std::path::Path,
    model_name: &str,
    tools: &[(&str, &str)],
    skills: &[crate::skills::Skill],
    identity_config: Option<&crate::config::IdentityConfig>,
    bootstrap_max_chars: Option<usize>,
    native_tools: bool,
    skills_prompt_mode: crate::config::SkillsPromptInjectionMode,
) -> String {
    // Unified prompt builder: the SAME `SystemPromptBuilder` the TUI/`Agent`
    // path uses, with `surface = Channel` so the surface-specific hint sections
    // (Hardware / Your Task / Channel Capabilities) and the timezone-only
    // datetime turn on while persona/identity/tools/safety/skills stay shared.
    //
    // Description-only tools `(name, desc)` are wrapped as `DescriptorTool` so
    // the one `ToolsSection` renders them (no `Parameters:` line for these).
    // Tool-call protocol instructions are appended by the caller via
    // `build_tool_instructions`, so `dispatcher_instructions` stays empty here.
    //
    // Resolve the active approval preset so SafetySection renders the
    // channel-accurate guidance (Strict really drops `shell` here after
    // PR3b-strict; Smart/Manual describe owner-approval, not the TUI's inline
    // Y/N/A). Failure to read the policy is non-fatal — `None` falls back to the
    // generic safety floor. The shell allowlist is intentionally NOT surfaced on
    // channels: the Layer-A approval manager gates non-read-only tools before
    // the Layer-B shell allowlist applies, so listing globs here would mislead.
    use crate::agent::prompt::{DescriptorTool, PromptContext, PromptSurface, SystemPromptBuilder};
    use crate::tools::Tool;

    let autonomy_preset = crate::profile::ProfileManager::active()
        .ok()
        .map(|profile| crate::approval::policy_writer::read_active_preset(&profile.policy_dir()))
        .unwrap_or(None);

    let stub_tools: Vec<Box<dyn Tool>> = tools
        .iter()
        .map(|(name, desc)| Box::new(DescriptorTool::new(*name, *desc)) as Box<dyn Tool>)
        .collect();

    let ctx = PromptContext {
        workspace_dir,
        model_name,
        surface: PromptSurface::Channel { native_tools },
        bootstrap_max_chars: bootstrap_max_chars.unwrap_or(BOOTSTRAP_MAX_CHARS),
        tools: &stub_tools,
        skills,
        skills_prompt_mode,
        identity_config,
        dispatcher_instructions: "",
        autonomy_preset,
        allowed_commands: &[],
    };

    let prompt = SystemPromptBuilder::with_defaults()
        .build(&ctx)
        .unwrap_or_default();

    if prompt.trim().is_empty() {
        "You are RantaiClaw, a fast and efficient AI assistant built in Rust. Be helpful, concise, and direct."
            .to_string()
    } else {
        prompt
    }
}
