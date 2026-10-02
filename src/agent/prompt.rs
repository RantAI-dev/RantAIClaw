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
/// hint sections (task/channel-capabilities) while keeping the
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

/// The `(name, description)` pairs a prompt lists for `tools`, read from the
/// tools themselves. Every door that lists tools takes its list from here, so
/// a tool is worded the same way in each prompt and a prompt names only the
/// tools its caller's registry holds.
#[must_use]
pub fn tool_descriptions(tools: &[Box<dyn Tool>]) -> Vec<(&str, &str)> {
    tools
        .iter()
        .map(|tool| (tool.name(), tool.description()))
        .collect()
}

/// Who a prompt is written for. See [`PromptContext::audience`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAudience {
    /// The owner: persona with the owner's name and timezone, the host and the
    /// workspace path, skill locations.
    Owner,
    /// A non-owner sender of a channel.
    Guest,
}

/// Whether the owner's files go into a prompt. `USER.md` and `MEMORY.md` are
/// memory read into the prompt, `BOOTSTRAP.md` carries the owner's name and
/// timezone from setup, and `TOOLS.md` carries the owner's SSH hosts and device
/// nicknames, so one rule decides all four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerFiles {
    Load,
    Omit,
}

impl OwnerFiles {
    /// The files load only for a turn that reads all of memory. A turn limited
    /// to one conversation, and a turn with no view, get none of them.
    #[must_use]
    pub fn for_view(view: Option<&crate::memory::MemoryView>) -> Self {
        match view {
            Some(crate::memory::MemoryView::All) => Self::Load,
            Some(crate::memory::MemoryView::Only(_)) | None => Self::Omit,
        }
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
    /// Who the prompt is written for. A guest prompt leaves out what describes
    /// the operator or the host, so none of it reaches a non-owner sender's
    /// context:
    ///   * the absolute workspace path, which carries the OS user name;
    ///   * the `Host:` line of the runtime section;
    ///   * the host's timezone, replaced by `UTC`;
    ///   * the owner's name in the persona, and the location of each skill.
    ///
    /// `AGENTS.md`, `SOUL.md` and `IDENTITY.md` render for both audiences: they
    /// describe the agent, not the operator.
    pub audience: PromptAudience,
    /// Whether `USER.md`, `MEMORY.md`, `BOOTSTRAP.md` and `TOOLS.md` go into
    /// the prompt. The door that builds the prompt sets it from the memory view
    /// of the turn the prompt is for ([`OwnerFiles::for_view`]). A guest
    /// prompt never carries them, whatever this says.
    pub owner_files: OwnerFiles,
}

impl PromptContext<'_> {
    fn is_guest(&self) -> bool {
        self.audience == PromptAudience::Guest
    }

    /// True when the four owner files go into this prompt.
    fn loads_owner_files(&self) -> bool {
        self.audience == PromptAudience::Owner && self.owner_files == OwnerFiles::Load
    }
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
                // surface they emit nothing, so the TUI prompt is unchanged.
                // On a Channel they add action framing and delivery hints.
                Box::new(TaskSection),
                Box::new(SafetySection),
                Box::new(SkillsSection),
                Box::new(MemorySection),
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
    let persona = match load_active_persona() {
        Some(p) => p,
        None => return String::new(),
    };
    wrap_persona_section(&persona.render())
}

/// Same as [`render_persona_section`], but for a guest turn: the owner's
/// name is replaced by "the user" (the person in the chat) and the timezone
/// is omitted, so neither reaches a non-owner sender. Role, tone and avoid
/// render the same as the owner's persona. Used by the channel dispatch
/// per-message persona splice (`replace_persona_section`) for turns from a
/// non-owner sender.
pub fn render_guest_persona_section() -> String {
    let persona = match load_active_persona() {
        Some(p) => p,
        None => return String::new(),
    };
    wrap_persona_section(&persona.render_for_guest())
}

/// Load the active profile's `persona.toml`, or `None` when no profile is
/// active or no persona is configured yet (fresh installs, headless tests).
fn load_active_persona() -> Option<crate::persona::PersonaToml> {
    let profile = crate::profile::ProfileManager::active().ok()?;
    crate::persona::read_persona_toml(&profile).ok().flatten()
}

/// Wrap a rendered persona body in the `## Persona` section header, or
/// return an empty string when the body is blank.
fn wrap_persona_section(rendered: &str) -> String {
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
pub struct MemorySection;
pub struct WorkspaceSection;
pub struct RuntimeSection;
pub struct DateTimeSection;
pub struct TaskSection;
pub struct ChannelCapabilitiesSection;

impl PromptSection for PersonaSection {
    fn name(&self) -> &str {
        "persona"
    }

    /// Inject the active profile's persona, rendered on the fly from
    /// `persona.toml` (there is no separate SYSTEM.md file). `personality set`
    /// reshapes the agent's voice for `agent -m`, `/api/v1/agent/chat`, and —
    /// via a per-message splice — running channel listeners.
    ///
    /// Resolution: read the active profile's persona.toml via the same
    /// reader the CLI uses. Fall through to an empty section when no
    /// persona is configured (fresh installs, headless tests, profile
    /// without a `persona/` dir) — silent rather than noisy.
    ///
    /// A guest audience renders the guest persona here too: the
    /// channel dispatch's per-message splice (`replace_persona_section`)
    /// overwrites this section on every turn anyway, but the guest prompt
    /// built once at channel start-up (`guest_system_prompt`) is this
    /// section's output until the first splice runs, so it must not carry
    /// the owner's name or timezone even briefly. The persona follows the
    /// audience alone: an owner whose turn reads one conversation keeps the
    /// owner persona and loses only the files.
    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        Ok(if ctx.is_guest() {
            render_guest_persona_section()
        } else {
            render_persona_section()
        })
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
                let loaded = identity::load_aieos_identity(config, ctx.workspace_dir);
                if let Err(ref e) = loaded {
                    tracing::warn!(error = %e, "aieos identity failed to load; falling back to workspace files");
                }
                if let Ok(Some(aieos)) = loaded {
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

        // Core identity files, injected (with a not-found marker if absent) on
        // every surface. `USER.md` (the owner's profile) and `TOOLS.md` (the
        // owner's SSH hosts and device nicknames) are owner files: they go in
        // only when the turn reads all of memory.
        let mut files = vec!["AGENTS.md", "SOUL.md", "TOOLS.md", "IDENTITY.md", "USER.md"];
        if !ctx.loads_owner_files() {
            files.retain(|file| !matches!(*file, "TOOLS.md" | "USER.md"));
        }
        for file in files {
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
        // marker so the absence is visible. The setup wizard writes the
        // owner's name and timezone into this file, so it is an owner file like
        // USER.md and MEMORY.md.
        if ctx.loads_owner_files()
            && (matches!(ctx.surface, PromptSurface::Agent)
                || ctx.workspace_dir.join("BOOTSTRAP.md").exists())
        {
            inject_workspace_file(
                &mut prompt,
                ctx.workspace_dir,
                "BOOTSTRAP.md",
                ctx.bootstrap_max_chars,
            );
        }

        // `MEMORY.md` is the projection of the owner's private notes. A turn
        // that reads one conversation, a guest's included, gets that
        // conversation's notes through the recall tier (`memory_recall` + the
        // dispatch memory-context injection) and not through this file.
        if ctx.loads_owner_files() {
            inject_workspace_file(
                &mut prompt,
                ctx.workspace_dir,
                "MEMORY.md",
                ctx.bootstrap_max_chars,
            );
        }

        Ok(prompt)
    }
}

impl PromptSection for ToolsSection {
    fn name(&self) -> &str {
        "tools"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        // A guest's tools are the gate's as reloaded per turn, so a guest
        // prompt built at start-up carries no list. See
        // [`render_guest_turn_sections`].
        if ctx.is_guest() {
            return Ok(String::new());
        }
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

/// Heading [`SafetySection`] emits. Callers that re-render the section into an
/// already-built prompt split on this, so it lives next to the code that writes
/// it rather than being duplicated as a literal at the call site.
pub const SAFETY_SECTION_HEADING: &str = "## Safety + Approval Policy";

/// Render just the safety section, for callers that cache an expensive base
/// prompt but need this part to track the live policy.
///
/// The channel path builds its system prompt once at startup — it reads
/// bootstrap files and skills off disk, so rebuilding per message is not free —
/// but the approval policy can change under a running daemon. Everything this
/// section reads is cheap and in memory, so it is re-rendered per turn and
/// spliced in by [`replace_safety_section`].
#[must_use]
pub fn render_safety_section(
    surface: PromptSurface,
    autonomy_preset: Option<crate::approval::policy_writer::PolicyPreset>,
    tools: &[Box<dyn Tool>],
    allowed_commands: &[String],
) -> String {
    render_safety(
        surface,
        autonomy_preset,
        tools,
        allowed_commands,
        PromptAudience::Owner,
    )
}

/// [`render_safety_section`] for a guest turn. `tools` is the guest's own
/// list, so the section promises the guest only what those tools give.
#[must_use]
pub fn render_guest_safety_section(
    surface: PromptSurface,
    autonomy_preset: Option<crate::approval::policy_writer::PolicyPreset>,
    tools: &[Box<dyn Tool>],
) -> String {
    render_safety(surface, autonomy_preset, tools, &[], PromptAudience::Guest)
}

fn render_safety(
    surface: PromptSurface,
    autonomy_preset: Option<crate::approval::policy_writer::PolicyPreset>,
    tools: &[Box<dyn Tool>],
    allowed_commands: &[String],
    audience: PromptAudience,
) -> String {
    let ctx = PromptContext {
        workspace_dir: Path::new("."),
        model_name: "",
        tools,
        surface,
        bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
        skills: &[],
        skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
        identity_config: None,
        dispatcher_instructions: "",
        autonomy_preset,
        allowed_commands,
        audience,
        owner_files: OwnerFiles::Omit,
    };
    SafetySection.build(&ctx).unwrap_or_default()
}

/// The read-only tools among `tools` that run without an approval gate, as the
/// phrase a safety text uses (`reading files, recalling memory`), or `None`
/// when `tools` holds none of them. A guest's tools are the operator's choice,
/// so the owner's fixed promise would claim reads the guest cannot make.
fn ungated_reads(tools: &[Box<dyn Tool>]) -> Option<String> {
    const READS: [(&str, &str); 2] = [
        ("file_read", "reading files"),
        ("memory_recall", "recalling memory"),
    ];
    let held: Vec<&str> = READS
        .iter()
        .filter(|(name, _)| tools.iter().any(|tool| tool.name() == *name))
        .map(|(_, what)| *what)
        .collect();
    (!held.is_empty()).then(|| held.join(", "))
}

/// The Strict-policy line that says what a guest can still do: the read tools
/// it has, and none it lacks. The operator sets a guest's tools, so the owner's
/// fixed list would promise reads the guest cannot make.
fn guest_strict_reads_line(tools: &[Box<dyn Tool>]) -> String {
    const READS: [(&str, &str); 3] = [
        ("file_read", "read files"),
        ("memory_recall", "recall memory"),
        ("web_search_tool", "search the web"),
    ];
    let can_still: Vec<String> = READS
        .iter()
        .filter(|(name, _)| tools.iter().any(|t| t.name() == *name))
        .map(|(name, what)| format!("{what} (`{name}`)"))
        .collect();
    if can_still.is_empty() {
        // A tool outside the list above may still be a read that runs, so the
        // denial is written only when the guest has no tool at all.
        return if tools.is_empty() {
            String::from(
                "- None of your tools run under this policy. Answer from the conversation.\n",
            )
        } else {
            String::new()
        };
    }
    format!("- You can still {}, and reason.\n", can_still.join(", "))
}

/// The `## Tools` and `## Your Task` sections of a guest turn, built from the
/// tools the reloaded gate permits (`guest_tools`). A guest prompt built at
/// start-up omits both, so an edit to `guest_allowed_tools` reaches the next
/// message. A guest with no tool gets no list and the task framing that says so.
#[must_use]
pub fn render_guest_turn_sections(guest_tools: &[Box<dyn Tool>], native_tools: bool) -> String {
    let mut out = String::new();
    if !guest_tools.is_empty() {
        out.push_str("## Tools\n\n");
        for tool in guest_tools {
            let _ = writeln!(out, "- **{}**: {}", tool.name(), tool.description());
        }
        out.push('\n');
    }
    out.push_str(task_framing(native_tools, !guest_tools.is_empty()));
    out.push('\n');
    out
}

/// Heading of the persona section, as emitted by [`render_persona_section`].
pub const PERSONA_SECTION_HEADING: &str = "## Persona";

/// Byte offset of the line that is exactly `heading`. A heading that only
/// begins a longer one, such as `## Persona` in `## Personality`, is not it.
fn find_heading_line(prompt: &str, heading: &str) -> Option<usize> {
    prompt.match_indices(heading).map(|(at, _)| at).find(|&at| {
        let starts_line = at == 0 || prompt[..at].ends_with('\n');
        let rest = &prompt[at + heading.len()..];
        let ends_line = rest.is_empty() || rest.starts_with('\n') || rest.starts_with("\r\n");
        starts_line && ends_line
    })
}

/// Swap the section opened by `heading` in an already-built prompt for
/// `replacement`. Returns `prompt` unchanged when it carries no such section,
/// so a caller cannot silently lose the rest of the prompt if section
/// composition changes.
#[must_use]
fn replace_section(prompt: &str, heading: &str, replacement: &str) -> String {
    let Some(start) = find_heading_line(prompt, heading) else {
        return prompt.to_string();
    };
    // Sections are joined with a blank line and each opens with `## `, so the
    // next such marker is the end of this one.
    let rest = &prompt[start + heading.len()..];
    let end = rest
        .find("\n## ")
        .map_or(prompt.len(), |i| start + heading.len() + i + 1);

    let mut out = String::with_capacity(prompt.len());
    out.push_str(&prompt[..start]);
    out.push_str(replacement.trim_end());
    if end < prompt.len() {
        out.push_str("\n\n");
        out.push_str(&prompt[end..]);
    }
    out
}

/// Swap the safety section of an already-built prompt for `replacement`.
#[must_use]
pub fn replace_safety_section(prompt: &str, replacement: &str) -> String {
    replace_section(prompt, SAFETY_SECTION_HEADING, replacement)
}

/// Swap the persona section of an already-built prompt for `replacement` (or
/// remove it when `replacement` is empty). Lets a channel re-render the persona
/// per message so a `PUT /api/v1/personality` reaches an already-running
/// listener without a restart.
#[must_use]
pub fn replace_persona_section(prompt: &str, replacement: &str) -> String {
    replace_section(prompt, PERSONA_SECTION_HEADING, replacement)
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
                // Plan-mode analog. Strict maps to `AutonomyLevel::ReadOnly`, so
                // the refusal is much broader than shell: every tool that gates
                // on `can_act()` is refused. Naming that up front is the
                // difference between the model planning around the limit and
                // the model promising a file write it cannot perform.
                out.push_str(
                    "**Active approval policy: Strict (read-only).**\n\n\
                     - Nothing that changes state will run. Writing files, \
                     fetching URLs, driving the browser, scheduling jobs, \
                     storing or forgetting memory, sending messages, opening \
                     SSH/PTY sessions, and installing skills are all refused by \
                     policy — not gated behind a prompt, refused. Do not offer \
                     to do them.\n",
                );
                // Derived from the registry actually handed to this turn, so
                // the prompt states what is true instead of hedging about a
                // list that might be stale.
                if ctx.tools.iter().any(|t| t.name() == "shell") {
                    out.push_str(
                        "- `shell` is listed but every command is refused under \
                         this policy. Do not call it.\n",
                    );
                } else {
                    out.push_str("- The shell tool is not available in this session.\n");
                }
                if ctx.is_guest() {
                    out.push_str(&guest_strict_reads_line(ctx.tools));
                } else {
                    out.push_str(
                        "- You can still read files (`file_read`), search the \
                         workspace, recall memory (`memory_recall`), search the \
                         web, inspect tasks, and reason.\n",
                    );
                }
                out.push_str(
                    "- For any task that would normally require running a \
                     command or writing a file, describe what you would do — \
                     the exact commands or the exact file content — and let the \
                     user apply it. Say plainly that the policy blocked you; \
                     never report an action as done when it was refused.\n\
                     - To leave Strict mode the user types `/autonomy smart` \
                     or `/autonomy off`. Don't suggest it unless they ask.\n",
                );
            }
            Some(PolicyPreset::Smart) if is_channel => {
                out.push_str("**Active approval policy: Smart (messaging channel).**\n\n");
                if let Some(reads) = ungated_reads(ctx.tools) {
                    let _ = writeln!(out, "- Read-only tools ({reads}) run automatically.");
                }
                out.push_str(
                    "- Any tool that runs commands or changes state requires \
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
                     this channel",
                );
                if let Some(reads) = ungated_reads(ctx.tools) {
                    let _ = write!(out, "; read-only tools ({reads}) are not gated");
                }
                out.push_str(
                    ".\n\
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
        // A skill's location is a host path, so a guest prompt lists skills
        // without it.
        if ctx.is_guest() {
            return Ok(crate::skills::skills_to_prompt_for_guest(
                ctx.skills,
                ctx.skills_prompt_mode,
            ));
        }
        Ok(crate::skills::skills_to_prompt_with_mode(
            ctx.skills,
            ctx.workspace_dir,
            ctx.skills_prompt_mode,
        ))
    }
}

/// Standing nudge to curate durable facts as they appear, instead of letting
/// them die with the session. The pre-compaction flush
/// (`Agent::flush_durable_memory`) is the safety net for facts that were never
/// saved mid-conversation; this section is the first line — the model saves a
/// fact the moment the user states it.
///
/// Self-gating, twice:
///   * only on [`PromptSurface::Agent`] — channel prompts serve guests too,
///     and a guest's words must not be nudged into durable memory (the same
///     taint boundary that keeps the flush off the channel auto-compaction
///     path);
///   * only when a `memory_store` tool is actually registered — nudging a
///     model toward a tool it does not have manufactures failed calls.
impl PromptSection for MemorySection {
    fn name(&self) -> &str {
        "memory"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        if ctx.surface != PromptSurface::Agent {
            return Ok(String::new());
        }
        if !ctx.tools.iter().any(|t| t.name() == "memory_store") {
            return Ok(String::new());
        }
        Ok("## Memory
            When the user states something durable — a preference, a standing             decision, a project fact, a correction — save it with `memory_store`             (category `core`, a descriptive snake_case key such as             `user_language`). Update the existing key, or pass `replaces` with a             phrase from the old entry, instead of piling up variants. Do not             save one-off conversational detail, secrets, or anything you are             unsure about — when in doubt, ask first.
"
            .to_string())
    }
}

impl PromptSection for WorkspaceSection {
    fn name(&self) -> &str {
        "workspace"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        // The absolute path contains the OS user name, and quick setup uses
        // that name as the owner's. A guest gets the relative form only.
        if ctx.is_guest() {
            return Ok(String::from(
                "## Workspace\n\nFile paths are relative to the bot's workspace.",
            ));
        }
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
        // The host name identifies the operator's machine; a guest does not
        // need it.
        if ctx.is_guest() {
            return Ok(format!(
                "## Runtime\n\nOS: {} | Model: {}",
                std::env::consts::OS,
                ctx.model_name
            ));
        }
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
        // The host's zone reveals where the operator is. A guest is told UTC.
        if ctx.is_guest() {
            return Ok(String::from("## Current Date & Time\n\nTimezone: UTC"));
        }
        let now = Local::now();
        // A channel prompt carries the timezone only, as the prior channel
        // builder did, so its text does not change from one message to the next.
        // The owner prompt is rebuilt per message and could carry the time, but
        // the interactive agent is the one surface that shows a full timestamp:
        // it rebuilds per session.
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
        // Same reason as `ToolsSection`: a guest's framing depends on the tools
        // the reloaded gate permits.
        if ctx.is_guest() {
            return Ok(String::new());
        }
        Ok(String::from(task_framing(native_tools, true)))
    }
}

/// The "Your Task" text. `has_tools` is false only for a guest whose gate
/// permits no tool: telling it to use tools, or to emit `<tool_call>` tags,
/// only produces calls that fail.
fn task_framing(native_tools: bool, has_tools: bool) -> &'static str {
    if !has_tools {
        "## Your Task\n\n\
         When the user sends a message, respond naturally and answer it directly from the conversation. You have no tools in this session.\n\
         Do NOT: summarize this configuration, describe your capabilities, or output step-by-step meta-commentary."
    } else if native_tools {
        "## Your Task\n\n\
         When the user sends a message, respond naturally. Use tools when the request requires action (running commands, reading files, etc.).\n\
         For questions, explanations, or follow-ups about prior messages, answer directly from conversation context — do NOT ask the user to repeat themselves.\n\
         Do NOT: summarize this configuration, describe your capabilities, or output step-by-step meta-commentary."
    } else {
        "## Your Task\n\n\
         When the user sends a message, ACT on it. Use the tools to fulfill their request.\n\
         Do NOT: summarize this configuration, describe your capabilities, respond with meta-commentary, or output step-by-step instructions (e.g. \"1. First... 2. Next...\").\n\
         Instead: emit actual <tool_call> tags when you need to act. Just do what they ask."
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
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
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
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
        };
        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();
        assert!(prompt.contains("## Tools"));
        assert!(prompt.contains("test_tool"));
        assert!(prompt.contains("instr"));
    }

    /// The nudge appears exactly when it can be acted on: Agent surface with a
    /// registered `memory_store`. Each gate has its own control below.
    #[test]
    fn memory_nudge_renders_on_agent_surface_with_memory_store() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(DescriptorTool::new(
            "memory_store",
            "store a memory",
        ))];
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
            autonomy_preset: None,
            allowed_commands: &[],
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
        };
        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();
        assert!(prompt.contains("## Memory"), "nudge missing: {prompt}");
        assert!(prompt.contains("memory_store"));
    }

    /// Channel prompts serve guests too — a guest's words must not be nudged
    /// into durable memory. Same fixture, Channel surface, nudge gone.
    #[test]
    fn memory_nudge_is_absent_on_channel_surface() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(DescriptorTool::new(
            "memory_store",
            "store a memory",
        ))];
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
            autonomy_preset: None,
            allowed_commands: &[],
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
        };
        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();
        assert!(
            !prompt.contains("## Memory"),
            "nudge must not render for channels: {prompt}"
        );
    }

    /// Nudging a model toward a tool it does not have manufactures failed
    /// calls. No `memory_store` in the registry, no nudge.
    #[test]
    fn memory_nudge_is_absent_without_the_memory_store_tool() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(TestTool)];
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
            autonomy_preset: None,
            allowed_commands: &[],
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
        };
        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();
        assert!(
            !prompt.contains("## Memory"),
            "nudge needs the tool: {prompt}"
        );
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
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
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

    /// Strict now refuses at the gate rather than unregistering the tool, so
    /// The channel prompt is cached at startup, so the safety block has to be
    /// swappable in place. Everything around it must survive intact.
    #[test]
    fn replace_safety_section_swaps_only_that_block() {
        let prompt = "## Persona\n\nbe nice\n\n## Safety + Approval Policy\n\nold policy text\n\n## Skills\n\nskill list\n";
        let out = replace_safety_section(prompt, "## Safety + Approval Policy\n\nnew policy text");

        assert!(
            out.contains("be nice"),
            "earlier sections must survive: {out}"
        );
        assert!(
            out.contains("skill list"),
            "later sections must survive: {out}"
        );
        assert!(out.contains("new policy text"));
        assert!(
            !out.contains("old policy text"),
            "the stale block must be gone: {out}"
        );
        assert_eq!(
            out.matches(SAFETY_SECTION_HEADING).count(),
            1,
            "must not duplicate the heading: {out}"
        );
    }

    /// `## Persona` is a prefix of the AIEOS heading `## Personality`. With no
    /// persona to swap in, a match on the prefix cuts the AIEOS block out of
    /// the prompt, so a heading matches only as a whole line.
    #[test]
    fn replace_persona_section_leaves_the_aieos_personality_heading_alone() {
        let prompt = "## Identity\n\nname\n\n## Personality\n\nbold\n\n## Skills\n\nskill list\n";
        assert_eq!(replace_persona_section(prompt, ""), prompt);
    }

    /// The whole-line match still finds the real section when an AIEOS
    /// `## Personality` block comes first.
    #[test]
    fn replace_persona_section_swaps_the_persona_block_after_a_personality_block() {
        let prompt =
            "## Personality\n\nbold\n\n## Persona\n\nold persona\n\n## Skills\n\nskill list\n";
        let out = replace_persona_section(prompt, "## Persona\n\nnew persona");

        assert!(out.contains("## Personality\n\nbold"), "{out}");
        assert!(out.contains("new persona"), "{out}");
        assert!(!out.contains("old persona"), "{out}");
        assert!(out.contains("skill list"), "{out}");
    }

    /// A prompt with no safety block must come back untouched rather than
    /// losing everything after a heading that was never there.
    #[test]
    fn replace_safety_section_is_a_noop_without_the_heading() {
        let prompt = "## Persona\n\nbe nice\n\n## Skills\n\nskill list\n";
        assert_eq!(
            replace_safety_section(prompt, "## Safety + Approval Policy\n\nx"),
            prompt
        );
    }

    /// The section is a pure function of the preset, which is what lets the
    /// channel path re-render it per turn instead of rebuilding the prompt.
    #[test]
    fn render_safety_section_tracks_the_preset() {
        use crate::approval::policy_writer::PolicyPreset;
        let tools: Vec<Box<dyn Tool>> = vec![];
        let surface = PromptSurface::Channel {
            native_tools: false,
        };

        let strict = render_safety_section(surface, Some(PolicyPreset::Strict), &tools, &[]);
        let off = render_safety_section(surface, Some(PolicyPreset::Off), &tools, &[]);

        assert!(strict.contains("Strict (read-only)"), "{strict}");
        assert!(!off.contains("Strict (read-only)"), "{off}");
        assert_ne!(
            strict, off,
            "a different preset must produce different guidance"
        );
    }

    /// Strict now refuses at the gate rather than unregistering the tool, so
    /// `shell` really is in the list. The prompt must say that plainly —
    /// telling the model a listed tool is absent invites it to report the
    /// wrong reason for a refusal.
    #[test]
    fn safety_section_strict_names_shell_as_listed_but_refused() {
        use crate::approval::policy_writer::PolicyPreset;
        let tools: Vec<Box<dyn Tool>> =
            vec![Box::new(DescriptorTool::new("shell", "run a command"))];
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
            autonomy_preset: Some(PolicyPreset::Strict),
            allowed_commands: &[],
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
        };
        let out = SafetySection.build(&ctx).unwrap();
        assert!(
            out.contains("is listed but every command is refused"),
            "with shell in the registry the text must own that: {out}"
        );
        assert!(
            !out.contains("not available in this session"),
            "must not claim shell is absent when it is listed: {out}"
        );
    }

    #[test]
    fn safety_section_strict_states_the_full_read_only_refusal() {
        use crate::approval::policy_writer::PolicyPreset;
        // Strict enforces `AutonomyLevel::ReadOnly`, so `can_act()` refuses far
        // more than shell. A prompt that only mentions shell leaves the model
        // offering file writes and fetches it cannot perform.
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
            autonomy_preset: Some(PolicyPreset::Strict),
            allowed_commands: &[],
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
        };
        let out = SafetySection.build(&ctx).unwrap();
        assert!(
            out.contains("Writing files"),
            "must name writes as refused, not just shell: {out}"
        );
        assert!(
            out.contains("refused by policy"),
            "must say refused rather than prompted: {out}"
        );
        // This context has an empty tool list, so the text must say so
        // outright rather than hedge about a registration that may be stale.
        assert!(
            out.contains("shell tool is not available in this session"),
            "with no shell in the registry the text must say so plainly: {out}"
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
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
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
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
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
            requires: crate::skills::SkillRequires::default(),
            install_recipes: Vec::new(),
            remote: false,
            origin: None,
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
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
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
            requires: crate::skills::SkillRequires::default(),
            install_recipes: Vec::new(),
            remote: false,
            origin: None,
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
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
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
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
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
            requires: crate::skills::SkillRequires::default(),
            install_recipes: Vec::new(),
            remote: false,
            origin: None,
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
            audience: PromptAudience::Owner,
            owner_files: OwnerFiles::Load,
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

    /// A guest prompt must not contain `USER.md` content. The owner's
    /// profile is private to the owner; the channel runtime builds the guest
    /// prompt via `build_system_prompt_with_mode` with `PromptAudience::Guest`,
    /// and the identity section omits the file in that branch.
    #[test]
    fn guest_prompt_omits_user_md() {
        let workspace =
            std::env::temp_dir().join(format!("rantaiclaw_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("USER.md"),
            "OWNER_PROFILE_CANARY_TOKEN_98271",
        )
        .unwrap();

        let tools: Vec<Box<dyn Tool>> = vec![];
        let ctx = PromptContext {
            workspace_dir: &workspace,
            model_name: "test-model",
            surface: PromptSurface::Channel {
                native_tools: false,
            },
            bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
            tools: &tools,
            skills: &[],
            skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
            identity_config: None,
            dispatcher_instructions: "",
            autonomy_preset: None,
            allowed_commands: &[],
            audience: PromptAudience::Guest,
            owner_files: OwnerFiles::Load,
        };

        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();
        assert!(
            !prompt.contains("OWNER_PROFILE_CANARY_TOKEN_98271"),
            "guest prompt must not contain USER.md content, got:\n{prompt}"
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    /// The owner's `MEMORY.md` is private to the owner. A guest
    /// prompt must not surface it under any section header. The owner
    /// prompt does — the test creates a workspace with MEMORY.md, then
    /// builds both the owner prompt and the guest prompt and asserts only
    /// the owner copy contains the canary.
    #[test]
    fn guest_prompt_omits_memory_md() {
        let workspace =
            std::env::temp_dir().join(format!("rantaiclaw_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("MEMORY.md"),
            "OWNER_MEMORY_CANARY_TOKEN_51297",
        )
        .unwrap();

        let tools: Vec<Box<dyn Tool>> = vec![];
        let owner_prompt = SystemPromptBuilder::with_defaults()
            .build(&PromptContext {
                workspace_dir: &workspace,
                model_name: "test-model",
                surface: PromptSurface::Channel {
                    native_tools: false,
                },
                bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
                tools: &tools,
                skills: &[],
                skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
                identity_config: None,
                dispatcher_instructions: "",
                autonomy_preset: None,
                allowed_commands: &[],
                audience: PromptAudience::Owner,
                owner_files: OwnerFiles::Load,
            })
            .unwrap();
        let guest_prompt = SystemPromptBuilder::with_defaults()
            .build(&PromptContext {
                workspace_dir: &workspace,
                model_name: "test-model",
                surface: PromptSurface::Channel {
                    native_tools: false,
                },
                bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
                tools: &tools,
                skills: &[],
                skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
                identity_config: None,
                dispatcher_instructions: "",
                autonomy_preset: None,
                allowed_commands: &[],
                audience: PromptAudience::Guest,
                owner_files: OwnerFiles::Load,
            })
            .unwrap();

        assert!(
            owner_prompt.contains("OWNER_MEMORY_CANARY_TOKEN_51297"),
            "owner prompt should contain MEMORY.md"
        );
        assert!(
            !guest_prompt.contains("OWNER_MEMORY_CANARY_TOKEN_51297"),
            "guest prompt must not contain MEMORY.md, got:\n{guest_prompt}"
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    /// The four owner files reach a prompt only for a turn that reads all of
    /// memory. A turn limited to one conversation, and a turn with no view, get
    /// none of them, and the same workspace still carries the other identity
    /// files, so the test is not passing on an empty fixture.
    #[test]
    fn owner_files_load_only_under_the_all_view() {
        let workspace =
            std::env::temp_dir().join(format!("rantaiclaw_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let canaries = [
            ("USER.md", "PROFILE_CANARY_TOKEN_30418"),
            ("MEMORY.md", "NOTES_CANARY_TOKEN_70215"),
            ("BOOTSTRAP.md", "BOOTSTRAP_CANARY_TOKEN_61873"),
            ("TOOLS.md", "TOOLS_CANARY_TOKEN_28046"),
            ("SOUL.md", "SOUL_CANARY_TOKEN_44190"),
        ];
        for (file, canary) in canaries {
            std::fs::write(workspace.join(file), canary).unwrap();
        }

        let tools: Vec<Box<dyn Tool>> = vec![];
        let build = |view: Option<&crate::memory::MemoryView>| {
            SystemPromptBuilder::with_defaults()
                .build(&PromptContext {
                    workspace_dir: &workspace,
                    model_name: "test-model",
                    surface: PromptSurface::Channel {
                        native_tools: false,
                    },
                    bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
                    tools: &tools,
                    skills: &[],
                    skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
                    identity_config: None,
                    dispatcher_instructions: "",
                    autonomy_preset: None,
                    allowed_commands: &[],
                    audience: PromptAudience::Owner,
                    owner_files: OwnerFiles::for_view(view),
                })
                .unwrap()
        };

        let under_all = build(Some(&crate::memory::MemoryView::All));
        for (file, canary) in canaries {
            assert!(under_all.contains(canary), "{file} missing:\n{under_all}");
        }

        let one_conversation = crate::memory::MemoryView::Only("conversation-a".to_string());
        for (label, view) in [
            ("one conversation", Some(&one_conversation)),
            ("no view", None),
        ] {
            let prompt = build(view);
            for (file, canary) in &canaries[..4] {
                assert!(
                    !prompt.contains(canary),
                    "{file} reached a prompt built for {label}:\n{prompt}"
                );
            }
            assert!(
                prompt.contains("SOUL_CANARY_TOKEN_44190"),
                "{label}: the other identity files stay:\n{prompt}"
            );
        }

        let _ = std::fs::remove_dir_all(workspace);
    }

    /// The persona follows the audience and nothing else. An owner whose turn
    /// reads one conversation loses the owner files and keeps the owner persona.
    #[test]
    fn an_owner_without_the_owner_files_keeps_the_owner_persona() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let home = std::env::temp_dir().join(format!(
            "rantaiclaw_prompt_test_home_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let _home = crate::test_env::HomeGuard::set(&home);
        let _profile_env =
            crate::test_env::EnvGuard::set("RANTAICLAW_PROFILE", "rt-prompt-persona-owner-only");

        let profile = crate::profile::ProfileManager::active().unwrap();
        crate::persona::write_persona_toml(
            &profile,
            &crate::persona::PersonaToml {
                preset: crate::persona::PresetId::Default,
                name: "Owner Name".to_string(),
                timezone: "Asia/Jakarta".to_string(),
                role: "general productivity and helpful assistance".to_string(),
                tone: "neutral".to_string(),
                avoid: None,
                always_on_kbs: Vec::new(),
            },
        )
        .unwrap();

        let workspace =
            std::env::temp_dir().join(format!("rantaiclaw_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();

        let tools: Vec<Box<dyn Tool>> = vec![];
        let prompt = SystemPromptBuilder::with_defaults()
            .build(&PromptContext {
                workspace_dir: &workspace,
                model_name: "test-model",
                surface: PromptSurface::Channel {
                    native_tools: false,
                },
                bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
                tools: &tools,
                skills: &[],
                skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
                identity_config: None,
                dispatcher_instructions: "",
                autonomy_preset: None,
                allowed_commands: &[],
                audience: PromptAudience::Owner,
                owner_files: OwnerFiles::Omit,
            })
            .unwrap();

        assert!(
            prompt.contains("Owner Name") && prompt.contains("Asia/Jakarta"),
            "an owner keeps the owner persona without the owner files:\n{prompt}"
        );
        assert!(
            !prompt.contains("assistant for the user"),
            "the guest persona reached an owner prompt:\n{prompt}"
        );

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `PersonaSection` itself must render the guest persona for a guest
    /// audience. The dispatch per-turn splice
    /// (`replace_persona_section`, tested in `channels::mod_tests`) fixes this
    /// up on every channel turn anyway, but the guest prompt built once at
    /// channel start-up (`guest_system_prompt`) is this section's own output
    /// until the first splice runs, so it must not carry the owner's name or
    /// timezone even briefly.
    #[test]
    fn persona_section_skips_owner_name_and_timezone_for_a_guest_audience() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let home = std::env::temp_dir().join(format!(
            "rantaiclaw_prompt_test_home_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&home).unwrap();
        let _home = crate::test_env::HomeGuard::set(&home);
        let _profile_env =
            crate::test_env::EnvGuard::set("RANTAICLAW_PROFILE", "rt-prompt-persona-guest");

        let profile = crate::profile::ProfileManager::active().unwrap();
        crate::persona::write_persona_toml(
            &profile,
            &crate::persona::PersonaToml {
                preset: crate::persona::PresetId::Default,
                name: "Owner Name".to_string(),
                timezone: "Asia/Jakarta".to_string(),
                role: "general productivity and helpful assistance".to_string(),
                tone: "neutral".to_string(),
                avoid: None,
                always_on_kbs: Vec::new(),
            },
        )
        .unwrap();

        let workspace =
            std::env::temp_dir().join(format!("rantaiclaw_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();

        let tools: Vec<Box<dyn Tool>> = vec![];
        let owner_prompt = SystemPromptBuilder::with_defaults()
            .build(&PromptContext {
                workspace_dir: &workspace,
                model_name: "test-model",
                surface: PromptSurface::Channel {
                    native_tools: false,
                },
                bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
                tools: &tools,
                skills: &[],
                skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
                identity_config: None,
                dispatcher_instructions: "",
                autonomy_preset: None,
                allowed_commands: &[],
                audience: PromptAudience::Owner,
                owner_files: OwnerFiles::Load,
            })
            .unwrap();
        let guest_prompt = SystemPromptBuilder::with_defaults()
            .build(&PromptContext {
                workspace_dir: &workspace,
                model_name: "test-model",
                surface: PromptSurface::Channel {
                    native_tools: false,
                },
                bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
                tools: &tools,
                skills: &[],
                skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
                identity_config: None,
                dispatcher_instructions: "",
                autonomy_preset: None,
                allowed_commands: &[],
                audience: PromptAudience::Guest,
                owner_files: OwnerFiles::Load,
            })
            .unwrap();

        assert!(
            owner_prompt.contains("Owner Name") && owner_prompt.contains("Asia/Jakarta"),
            "control: the owner prompt must carry the persona's name and timezone:\n{owner_prompt}"
        );
        assert!(
            !guest_prompt.contains("Owner Name"),
            "guest prompt must not name the owner, got:\n{guest_prompt}"
        );
        assert!(
            !guest_prompt.contains("Asia/Jakarta"),
            "guest prompt must not carry the owner's timezone, got:\n{guest_prompt}"
        );

        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Tests `IdentitySection` directly — assert it omits the
    /// "USER.md" header for a guest audience, even if the file
    /// does not exist on disk. The not-found marker is also owner-private.
    #[test]
    fn identity_section_marks_user_md_when_owner() {
        let workspace =
            std::env::temp_dir().join(format!("rantaiclaw_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();

        let tools: Vec<Box<dyn Tool>> = vec![];
        let owner = IdentitySection
            .build(&PromptContext {
                workspace_dir: &workspace,
                model_name: "test-model",
                surface: PromptSurface::Channel {
                    native_tools: false,
                },
                bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
                tools: &tools,
                skills: &[],
                skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
                identity_config: None,
                dispatcher_instructions: "",
                autonomy_preset: None,
                allowed_commands: &[],
                audience: PromptAudience::Owner,
                owner_files: OwnerFiles::Load,
            })
            .unwrap();
        let guest = IdentitySection
            .build(&PromptContext {
                workspace_dir: &workspace,
                model_name: "test-model",
                surface: PromptSurface::Channel {
                    native_tools: false,
                },
                bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
                tools: &tools,
                skills: &[],
                skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
                identity_config: None,
                dispatcher_instructions: "",
                autonomy_preset: None,
                allowed_commands: &[],
                audience: PromptAudience::Guest,
                owner_files: OwnerFiles::Load,
            })
            .unwrap();

        assert!(
            owner.contains("USER.md"),
            "owner identity section should mention USER.md (even as not-found marker)"
        );
        assert!(
            !guest.contains("USER.md"),
            "guest identity section must not mention USER.md at all, got:\n{guest}"
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    /// The setup wizard writes the owner's name and timezone into
    /// `BOOTSTRAP.md`. A guest prompt must not carry that file; the owner
    /// prompt still does when the file exists on disk.
    #[test]
    fn guest_prompt_omits_bootstrap_md() {
        let workspace =
            std::env::temp_dir().join(format!("rantaiclaw_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("BOOTSTRAP.md"),
            "Your human's name is **Owner Name** (timezone: Asia/Jakarta)",
        )
        .unwrap();

        let tools: Vec<Box<dyn Tool>> = vec![];
        let owner_prompt = SystemPromptBuilder::with_defaults()
            .build(&PromptContext {
                workspace_dir: &workspace,
                model_name: "test-model",
                surface: PromptSurface::Channel {
                    native_tools: false,
                },
                bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
                tools: &tools,
                skills: &[],
                skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
                identity_config: None,
                dispatcher_instructions: "",
                autonomy_preset: None,
                allowed_commands: &[],
                audience: PromptAudience::Owner,
                owner_files: OwnerFiles::Load,
            })
            .unwrap();
        let guest_prompt = SystemPromptBuilder::with_defaults()
            .build(&PromptContext {
                workspace_dir: &workspace,
                model_name: "test-model",
                surface: PromptSurface::Channel {
                    native_tools: false,
                },
                bootstrap_max_chars: BOOTSTRAP_MAX_CHARS,
                tools: &tools,
                skills: &[],
                skills_prompt_mode: crate::config::SkillsPromptInjectionMode::Full,
                identity_config: None,
                dispatcher_instructions: "",
                autonomy_preset: None,
                allowed_commands: &[],
                audience: PromptAudience::Guest,
                owner_files: OwnerFiles::Load,
            })
            .unwrap();

        assert!(
            owner_prompt.contains("### BOOTSTRAP.md"),
            "owner prompt should contain BOOTSTRAP.md, got:\n{owner_prompt}"
        );
        assert!(
            !guest_prompt.contains("### BOOTSTRAP.md"),
            "guest prompt must not contain BOOTSTRAP.md, got:\n{guest_prompt}"
        );

        let _ = std::fs::remove_dir_all(workspace);
    }
}
