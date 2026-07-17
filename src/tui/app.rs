use std::io::{self, IsTerminal, Stdout, Write};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, List, ListItem, Paragraph, Widget, Wrap},
    Terminal, TerminalOptions, Viewport,
};

use tokio::sync::mpsc;

use super::async_bridge::{TuiAgentActor, TurnRequest};
use super::commands::{CommandRegistry, CommandResult as CmdResult};
use super::context::{TokenUsage, TuiContext};
use super::TuiConfig;
use crate::agent::agent::Agent;
use crate::agent::events::{AgentEvent, AgentEventSender};
use crate::sessions::SessionStore;

/// Per-tool-call accumulation state used while streaming.
#[derive(Debug, Clone)]
pub struct ToolBlockState {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub result: Option<(bool, String)>, // (ok, preview)
    /// When this tool call started, for the working-indicator's
    /// per-tool elapsed timer. Set when we see `ToolCallStart` and
    /// frozen at `ToolCallEnd` time (still readable; just not the
    /// "current" tool any more).
    pub started_at: std::time::Instant,
}

/// Current application state.
#[derive(Debug, Default)]
pub enum AppState {
    #[default]
    Ready,
    Streaming {
        partial: String,
        tool_blocks: Vec<ToolBlockState>,
        cancelling: bool,
        /// When the user's turn started, used by the working
        /// indicator's elapsed counter when no tool is in flight.
        turn_started_at: std::time::Instant,
    },
    Quitting,
}

/// Result of processing one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventResult {
    Continue,
    Quit,
}

/// Live handle to the background channel runtime spawned by the TUI.
///
/// Cancelling `shutdown` makes every supervised listener stop and
/// `start_channels_with_cancellation` return; `handle` lets a restart drain
/// the previous runtime before the new one binds the same backend (Telegram
/// `getUpdates` is single-consumer, so overlapping pollers would 409).
pub struct ChannelSupervisor {
    shutdown: tokio_util::sync::CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

/// Top-level TUI application.
// Many independent UI mode flags; grouping them into a sub-struct would not
// improve clarity and would churn every call site.
#[allow(clippy::struct_excessive_bools)]
pub struct TuiApp {
    pub state: AppState,
    pub context: TuiContext,
    pub command_registry: CommandRegistry,
    /// Active config, cloned for provisioner use.
    pub config: crate::config::Config,
    /// Active profile, cloned for provisioner use.
    pub profile: crate::profile::Profile,
    /// Slash-command dropdown — visible whenever the input buffer starts
    /// with `/`. Filtered by prefix on every keystroke.
    pub autocomplete: super::widgets::autocomplete::Autocomplete,
    /// Active modal overlay (e.g. /help). `None` = no overlay shown.
    /// Esc dismisses; left/right arrows cycle tabs.
    pub overlay: Option<super::commands::OverlayContent>,
    /// Active setup provisioner overlay. When `Some`, the chat input is
    /// suppressed and key events route to the overlay. The provisioner
    /// runs on a tokio task and emits `ProvisionEvent`s received here.
    pub setup_overlay: Option<super::SetupOverlayState>,
    /// Receiver for events emitted by the active provisioner.
    pub setup_event_rx:
        Option<tokio::sync::mpsc::Receiver<crate::onboard::provision::ProvisionEvent>>,
    /// Sender for responses (prompt answers) back to the provisioner.
    pub setup_response_tx:
        Option<tokio::sync::mpsc::Sender<crate::onboard::provision::ProvisionResponse>>,
    /// Receiver for the post-save `Config` from the active provisioner's
    /// spawn task. Drained in the render tick; on receive we swap
    /// `self.config` to the saved value so the *next* provisioner clone
    /// sees the prior provisioner's writes.
    ///
    /// Without this, the first-run wizard had a race: provisioner N's
    /// spawn task emitted `Done` (via the provisioner) BEFORE its async
    /// `config.save().await` completed; provisioner N+1's overlay then
    /// cloned a stale `self.config` and saved its mutated clone back
    /// over N's writes. Net effect: only the last provisioner's writes
    /// landed on disk, wiping everything else (provider, api_key, ...).
    /// Fix shipped in v0.6.56.
    pub setup_save_complete_rx: Option<tokio::sync::oneshot::Receiver<crate::config::Config>>,
    /// Active interactive list picker — Up/Down/Enter/Esc overlay used
    /// by `/model`, `/sessions`, `/resume`, `/personality`, etc. The
    /// `ListPicker.kind` tag tells the Enter handler what to do with the
    /// selected key. `None` when no picker is open.
    pub list_picker: Option<super::widgets::ListPicker>,
    /// Last query we kicked off a ClawHub server-side search for. Used to
    /// detect when the picker's query has changed since the last fetch so
    /// we can fire a fresh `clawhub::search` and replace results live.
    pub clawhub_install_last_query: String,
    /// Monotonic version counter for ClawHub search tasks. Each task tags
    /// its result with the version it was spawned for; the receiver only
    /// applies results matching the current version, so stale completions
    /// from rapid typing are dropped instead of overwriting newer results.
    pub clawhub_install_search_version: u64,
    /// Channel for ClawHub search results posted back from spawned tasks.
    /// `None` when the install picker isn't open.
    pub clawhub_install_results_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<(
            u64,
            anyhow::Result<Vec<crate::skills::clawhub::ClawHubSkill>>,
        )>,
    >,
    /// Sender side of the results channel above. Cloned per spawned task.
    pub clawhub_install_results_tx: Option<
        tokio::sync::mpsc::UnboundedSender<(
            u64,
            anyhow::Result<Vec<crate::skills::clawhub::ClawHubSkill>>,
        )>,
    >,
    /// Slug currently being installed from ClawHub, plus the spinner frame
    /// index (advanced each render tick). `None` when no install is in
    /// flight. While `Some`, the picker title shows a Braille-spinner
    /// "Installing …" line that animates per tick.
    pub clawhub_install_in_progress: Option<(String, usize)>,
    /// Completion channel for spawned ClawHub install tasks. `Ok(slug)` on
    /// success, `Err(message)` on failure. The render loop drains this
    /// each tick — when a result arrives, the install picker swaps to the
    /// Skill picker (success) or its title flips to an error string
    /// (failure).
    pub clawhub_install_completion_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<Result<String, String>>>,
    pub clawhub_install_completion_tx:
        Option<tokio::sync::mpsc::UnboundedSender<Result<String, String>>>,
    /// Skill currently running install-deps from the local `/skills`
    /// picker, plus spinner frame. Triggered by Ctrl+I/Tab on a skill row.
    pub skill_deps_install_in_progress: Option<(String, usize)>,
    pub skill_deps_install_completion_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<
            Result<crate::skills::install_deps::InstallDepsOutcome, String>,
        >,
    >,
    pub skill_deps_install_completion_tx: Option<
        tokio::sync::mpsc::UnboundedSender<
            Result<crate::skills::install_deps::InstallDepsOutcome, String>,
        >,
    >,
    pub skill_deps_install_finished_at: Option<Instant>,
    /// Background watcher for profile/workspace skill edits. The watcher
    /// owns the OS file handle; the TUI drains debounced reload ticks.
    pub skills_watcher: Option<crate::skills::watcher::SkillsWatcher>,
    /// Background watcher for the active profile's `config.toml`.
    /// Direct edits (user adds an `[mcp_servers.foo]` block, swaps the
    /// provider, changes the model) trigger a debounced reload tick;
    /// the TUI drains it each frame and runs the same `reload_config`
    /// pipeline that wizard close uses, so the agent picks up the
    /// change on the next turn without a restart.
    pub config_watcher: Option<crate::config::watcher::ConfigWatcher>,
    /// Handle to the background channel runtime (Telegram/Discord/Slack/…
    /// listeners) spawned alongside the TUI. Held so a mid-session channel
    /// or skill change can cancel and respawn the runtime in place via
    /// `restart_channels` — newly-configured channels start polling and the
    /// rebuilt system prompt picks up new skills without closing the TUI.
    pub channel_supervisor: Option<ChannelSupervisor>,
    /// True while the install picker was opened from inside the first-run
    /// wizard's skills step. On picker close (Esc), we send
    /// `ProvisionResponse::InstalledSkills(installed_slugs)` back to the
    /// wizard's response channel so the wizard can advance.
    pub wizard_install_in_progress: bool,
    /// Slugs successfully installed during the current install-picker
    /// session. Reset when the picker opens; consumed when it closes
    /// during a wizard install step.
    pub wizard_installed_slugs: Vec<String>,
    /// Read-only info panel (channels / config / doctor / insights / status
    /// / usage / skill). Mutually exclusive with `list_picker` and
    /// `setup_overlay` — the key handler refuses to open one while another
    /// modal is up. v0.6.8 introduced this surface.
    pub info_panel: Option<super::widgets::InfoPanel>,
    /// Inline-mode scrollback queue. The event loop drains this list
    /// before each frame and emits each entry into the terminal's native
    /// scrollback above the viewport. Each entry is `(role, content)`.
    pub scrollback_queue: Vec<(String, String)>,
    /// Bytes of the current streaming `partial` already flushed to
    /// scrollback. Used to stream assistant output line-by-line into
    /// terminal scrollback while the turn is still in progress. Reset
    /// each time a new turn starts.
    pub stream_committed_chars: usize,
    /// Whether the `Assistant: ` header line for the current streaming
    /// turn has already been written to scrollback. Reset per turn.
    pub stream_header_committed: bool,
    /// `true` when Ctrl+G was pressed and the run loop should suspend
    /// the terminal, hand control to `$EDITOR`, and copy the resulting
    /// file contents back into the input buffer. The key handler can't
    /// run the editor itself because it doesn't own the `Terminal`.
    pub editor_request: bool,
    /// `true` when the run loop should wipe the terminal's screen and
    /// scrollback before the next render (e.g. after `/new`/`/clear`).
    /// Set by command handlers via `CommandResult::ClearTerminal` and
    /// consumed once the wipe is performed.
    pub clear_terminal_request: bool,
    /// First-run wizard. When `Some`, the app renders the wizard
    /// instead of the normal chat UI. Provisioner steps use the
    /// existing `setup_overlay` mechanism.
    pub first_run_wizard: Option<super::FirstRunWizard>,
    /// Console login gate — when set, owns the screen and absorbs all input
    /// until the operator's password verifies (armed when `config.gateway.login`
    /// has a password hash).
    pub login_gate: Option<super::LoginGateState>,
    /// Broadcast receiver for pending-approval notifications from the
    /// shared `PendingApprovals` registry on `SecurityPolicy`. Drained
    /// each frame; new requests surface as system messages instructing
    /// the user to type `/allow` or `/deny`. `None` until `run_tui`
    /// subscribes (test contexts skip this).
    pub pending_approvals_rx:
        Option<tokio::sync::broadcast::Receiver<crate::security::PendingRequest>>,
    /// Number of `Command not allowed by security policy` results seen
    /// in the current turn. Reset on every `finalize_turn` /
    /// `finalize_error`. Used to surface a one-shot "switch to
    /// /autonomy off" toast when a skill bootstrap repeatedly hits
    /// the Smart gate.
    pub shell_blocks_this_turn: u32,
    /// Whether the autonomy-hint toast has already fired this turn so
    /// we don't spam the user with the same suggestion on every block.
    pub autonomy_hint_shown_this_turn: bool,
    /// Most-recent unresolved pending approval. When `Some`, the input
    /// box surfaces a single-key Y/N/A prompt and absorbs Y/N/A/Esc so
    /// the user doesn't have to type `/allow X`. Resolves via
    /// `security.pending().resolve_by_basename(...)` and clears here.
    pub pending_approval: Option<crate::security::PendingRequest>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetupTopicAction {
    TuiProvisioner(String),
    /// Drill down into a category sub-picker. The string is the
    /// category key (`core`, `channel`, `integration`, `runtime`,
    /// `hardware`, `routing`).
    OpenCategorySubPicker(String),
    Unknown,
}

pub fn dispatch_setup_topic_key(key: &str) -> SetupTopicAction {
    if let Some(cat) = key.strip_prefix("cat:") {
        return SetupTopicAction::OpenCategorySubPicker(cat.to_string());
    }
    if crate::onboard::provision::provisioner_for(key).is_some() {
        return SetupTopicAction::TuiProvisioner(key.to_string());
    }
    SetupTopicAction::Unknown
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetupChannelAction {
    TuiProvisioner(String),
    Unknown,
}

pub fn dispatch_setup_channel_key(key: &str) -> SetupChannelAction {
    if crate::onboard::provision::provisioner_for(key).is_some() {
        return SetupChannelAction::TuiProvisioner(key.to_string());
    }
    SetupChannelAction::Unknown
}

impl TuiApp {
    fn refresh_available_skills(&mut self) {
        self.context.available_skills =
            crate::skills::load_skills_with_config(&self.config.workspace_dir, &self.config);
        self.context.available_skills_with_status =
            crate::skills::load_skills_with_status(&self.config.workspace_dir, &self.config);
    }

    fn skill_picker_items(&self) -> Vec<crate::tui::widgets::ListPickerItem> {
        self.context
            .available_skills_with_status
            .iter()
            .map(|(s, reasons)| {
                let primary = if s.version.is_empty() {
                    s.name.clone()
                } else {
                    format!("{} · v{}", s.name, s.version)
                };
                let primary = if reasons.is_empty() {
                    primary
                } else {
                    format!("✗ {primary}")
                };
                let mut secondary = s.description.clone();
                if !reasons.is_empty() {
                    let reason = reasons.join("; ");
                    secondary = if secondary.is_empty() {
                        format!("gated: {reason}")
                    } else {
                        format!("{secondary}  · gated: {reason}")
                    };
                }
                if !s.tags.is_empty() {
                    secondary = format!("{secondary}  ({})", s.tags.join(", "));
                }
                let has_missing_bin = s
                    .requires
                    .unmet()
                    .iter()
                    .any(|reason| reason.starts_with("missing binary"));
                if has_missing_bin && !s.install_recipes.is_empty() {
                    secondary = if secondary.is_empty() {
                        "Ctrl+I install deps".to_string()
                    } else {
                        format!("{secondary}  · Ctrl+I install deps")
                    };
                }
                crate::tui::widgets::ListPickerItem {
                    key: s.name.clone(),
                    primary,
                    secondary,
                }
            })
            .collect()
    }

    /// Create a new `TuiApp`, starting or resuming a session based on config.
    ///
    /// `req_tx` and `events_rx` are the TUI-side ends of the bridge to the
    /// `TuiAgentActor`. The actor owns the paired `req_rx`/`events_tx` and is
    /// spawned by `run_tui` before the app is constructed.
    pub fn new(
        tui_config: &TuiConfig,
        config: crate::config::Config,
        profile: crate::profile::Profile,
        req_tx: mpsc::Sender<TurnRequest>,
        events_rx: mpsc::Receiver<AgentEvent>,
    ) -> Result<Self> {
        std::fs::create_dir_all(&tui_config.data_dir)?;
        let db_path = tui_config.data_dir.join("sessions.db");
        let store = SessionStore::open(&db_path)?;
        // Best-effort one-shot: derive titles for legacy sessions that
        // never went through the auto-titling path. Idempotent — a no-op
        // once every session has a title.
        let _ = store.backfill_titles();

        let mut context = TuiContext::new(
            store,
            &tui_config.model,
            tui_config.resume_session.as_deref(),
            req_tx,
            events_rx,
        )?;

        // Snapshot of every registered command so /help can build its
        // picker without reaching back into TuiApp from the command
        // handler (which only sees TuiContext).
        let command_registry = CommandRegistry::new();
        context.available_commands = command_registry
            .get_help()
            .into_iter()
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect();

        Ok(Self {
            state: AppState::Ready,
            context,
            command_registry,
            config,
            profile,
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_save_complete_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            clawhub_install_last_query: String::new(),
            clawhub_install_search_version: 0,
            clawhub_install_results_rx: None,
            clawhub_install_results_tx: None,
            clawhub_install_in_progress: None,
            clawhub_install_completion_rx: None,
            clawhub_install_completion_tx: None,
            skill_deps_install_in_progress: None,
            skill_deps_install_completion_rx: None,
            skill_deps_install_completion_tx: None,
            skill_deps_install_finished_at: None,
            skills_watcher: None,
            config_watcher: None,
            channel_supervisor: None,
            wizard_install_in_progress: false,
            wizard_installed_slugs: Vec::new(),
            info_panel: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
            login_gate: None,
            pending_approvals_rx: None,
            shell_blocks_this_turn: 0,
            autonomy_hint_shown_this_turn: false,
            pending_approval: None,
        })
    }

    /// Re-evaluate the slash-command dropdown against the current input
    /// buffer. Called after every keystroke that mutates `input_buffer`.
    fn refresh_autocomplete(&mut self) {
        let buf = &self.context.input_buffer;
        if buf.starts_with('/') && !buf.contains(' ') && !buf.contains('\n') {
            let suggestions = self
                .command_registry
                .autocomplete_with_descriptions_and_skills(buf, &self.context.available_skills);
            self.autocomplete.update(suggestions);
        } else {
            self.autocomplete.hide();
        }
    }

    /// Advance the approval-policy preset for the active profile by one
    /// step (`Manual → Smart → Strict → Off → Manual`) and persist it
    /// to `<policy_dir>/{autonomy,command_allowlist,forbidden_paths}.toml`.
    ///
    /// Wired to `KeyCode::BackTab` (Shift+Tab) and shared with the
    /// `/autonomy` slash command (which calls into the same write path).
    /// The on-disk write goes through `policy_writer::write_policy_files`
    /// with `force=true` — that's the same call `rantaiclaw setup
    /// approvals --force` makes, so any hand-edits to
    /// `command_allowlist.toml` / `forbidden_paths.toml` are clobbered.
    /// `runtime_allowlist.toml` (the user's `/allow X --persist`
    /// accretions) lives in a separate file and is preserved.
    fn cycle_autonomy_preset(&mut self) {
        use crate::approval::policy_writer::{self, PolicyPreset};
        let dir = self.profile.policy_dir();
        let current = policy_writer::read_active_preset(&dir).unwrap_or(PolicyPreset::Smart);
        let next = current.next();
        let warning = match policy_writer::write_policy_files(&self.profile, next, true) {
            Ok(w) => w,
            Err(e) => {
                let _ = self
                    .context
                    .append_system_message(&format!("✗ Failed to switch autonomy mode: {e}"));
                return;
            }
        };
        // Propagate to config.toml + live agent. Without this step the
        // preset file changes but `SecurityPolicy.autonomy` keeps its
        // launch-time value (v0.6.49 bug — Off didn't actually disable
        // the gate). `apply_preset_to_config_and_reload` saves
        // config.toml synchronously via block_in_place and asks the
        // actor to rebuild the agent with the new policy.
        if let Err(e) = self.apply_preset_to_config_and_reload(next) {
            let _ = self
                .context
                .append_system_message(&format!("⚠ Preset written, but live reload failed: {e}"));
        }
        self.context.autonomy_preset = Some(next);
        let _ = self.context.append_system_message(&format!(
            "⚙ Autonomy mode → {} ({}). Shift+Tab to cycle · /autonomy to pick.",
            next.label(),
            preset_blurb(next),
        ));
        if let Some(w) = warning {
            let _ = self.context.append_system_message(w);
        }
    }

    /// Apply `preset` to `self.config.autonomy.level`, save config.toml,
    /// and send the rebuilt config to the agent actor so the next turn
    /// uses the new `SecurityPolicy`. Returns the underlying error if
    /// saving or reloading fails — caller surfaces it as a system
    /// message but does not bail the TUI.
    fn apply_preset_to_config_and_reload(
        &mut self,
        preset: crate::approval::policy_writer::PolicyPreset,
    ) -> Result<()> {
        crate::approval::policy_writer::apply_preset_to_config(&mut self.config, preset);
        // Save is async (encrypts secrets); drive it on the current
        // tokio runtime via block_in_place. Same pattern `/memory` uses.
        let config_for_save = self.config.clone();
        let saved = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(config_for_save.save())
        });
        saved?;
        // Hand the new config to the agent actor — mirrors what
        // `reload_config` does after a wizard run, but without the
        // wholesale re-read since we already have the live struct.
        let req_tx = self.context.req_tx.clone();
        let config = self.config.clone();
        tokio::spawn(async move {
            let _ = req_tx
                .send(crate::tui::TurnRequest::Reload(Box::new(config)))
                .await;
        });
        Ok(())
    }

    /// Resolve the current pending approval inline via Y/N/A keystroke.
    /// `Session` allows the basename for this session only; `Persist`
    /// writes it to runtime_allowlist.toml; `Deny` rejects the call
    /// **and cancels the entire turn** so the LLM doesn't loop on
    /// alternative commands after the user has said no. Clears
    /// `self.pending_approval` regardless of outcome so the prompt
    /// overlay disappears.
    ///
    /// The "deny cancels everything" semantics (vs CC's "deny returns
    /// to LLM") came from tester feedback: under the per-call-deny
    /// model the LLM kept trying alternative invocations (`curl -s` →
    /// blocked → `curl --head` → blocked → …) and burned a turn
    /// before giving up. One decision, one outcome.
    fn resolve_pending_approval(&mut self, decision: crate::security::Decision) {
        let req = match self.pending_approval.take() {
            Some(r) => r,
            None => return,
        };
        let basename = req.basename.clone();
        let Some(security) = self.context.security.as_ref() else {
            let _ = self
                .context
                .append_system_message("Security policy not available — cannot resolve approval.");
            return;
        };
        let Some(pending) = security.pending() else {
            let _ = self
                .context
                .append_system_message("Pending-approval registry not attached — cannot resolve.");
            return;
        };
        // Persist / Session decisions also add the basename to the
        // runtime allowlist so the same command doesn't prompt twice
        // in this session. Mirrors what /allow does.
        let persist_flag = matches!(decision, crate::security::Decision::Persist);
        if matches!(
            decision,
            crate::security::Decision::Session | crate::security::Decision::Persist
        ) {
            if let Err(e) = security.add_runtime_command(&basename, persist_flag) {
                tracing::warn!(
                    target: "tui",
                    error = %e,
                    basename = %basename,
                    "inline approval add_runtime_command failed"
                );
            }
        }
        let resolved = pending.resolve_by_basename(&basename, decision).is_some();

        // Deny → cancel the entire turn. Without this, the shell tool
        // returns the error to the LLM which typically reacts by
        // trying alternative commands (each hitting the gate again).
        // The user already said no; respect that as a "stop, don't
        // explore alternatives" signal. Sent as a non-blocking
        // try_send so resolve never stalls if the actor channel is
        // back-pressured.
        if matches!(decision, crate::security::Decision::Deny) {
            // Mark the streaming state as cancelling so the status bar
            // reflects the in-flight cancel immediately. The actor
            // will catch TurnRequest::Cancel and fire the token; the
            // outer agent loop already produces a clean Done event.
            if let AppState::Streaming { cancelling, .. } = &mut self.state {
                *cancelling = true;
            }
            let req_tx = self.context.req_tx.clone();
            tokio::spawn(async move {
                let _ = req_tx.send(crate::tui::TurnRequest::Cancel).await;
            });
        }

        let msg = match decision {
            crate::security::Decision::Session if resolved => {
                format!("✓ Approved `{basename}` for this session.")
            }
            crate::security::Decision::Persist if resolved => {
                format!("✓ Approved `{basename}` and persisted to allowlist.")
            }
            crate::security::Decision::Deny => {
                format!("✗ Denied `{basename}` — turn cancelled.")
            }
            _ => {
                format!("⚠ `{basename}` was no longer pending (timed out or already resolved).")
            }
        };
        let _ = self.context.append_system_message(&msg);
        self.scrollback_queue.push(("system".into(), msg));
    }

    /// Replace the input buffer with the highlighted command name.
    fn complete_selected_command(&mut self) {
        if let Some(name) = self.autocomplete.selected() {
            self.context.input_buffer = format!("{name} ");
            self.context.cursor_to_end();
            self.autocomplete.hide();
        }
    }

    /// Process a single terminal event, returning whether to continue or quit.
    pub async fn handle_event(&mut self, event: Event) -> Result<EventResult> {
        match event {
            Event::Key(key) => {
                // Windows + terminals that enable the kitty keyboard protocol
                // emit BOTH Press and Release for every keystroke. Without
                // this guard each char is pushed twice into the input buffer.
                // Linux ttys without the protocol only emit Press, so the
                // filter is a no-op there.
                if key.kind != KeyEventKind::Press {
                    return Ok(EventResult::Continue);
                }
                self.handle_key(key).await
            }
            // Bracketed-paste payload: insert verbatim at the cursor.
            // Modals (setup overlay, wizard, picker) absorb their own
            // text via dedicated handlers; pasting into them is rare
            // enough that we keep the input-buffer path simple and
            // route paste to the chat composer regardless. The pasted
            // text may contain literal newlines — those are preserved
            // in the buffer (multi-line prompt) instead of triggering
            // submit, which is the whole point of bracketed paste.
            Event::Paste(text) => {
                // Route a bracketed-paste payload into the active setup-overlay
                // prompt (e.g. pasting an API key during onboarding / the
                // first-run wizard, which delegates its prompts to the same
                // overlay). Without this, pasting into a setup prompt was
                // silently dropped on terminals that emit `Event::Paste`,
                // making API keys impossible to paste. Otherwise the paste
                // lands in the chat composer as before.
                if let Some(o) = self.setup_overlay.as_mut() {
                    if o.active_prompt().is_some() {
                        // Single-line secret/URL fields: strip line breaks so a
                        // trailing newline from the copy doesn't corrupt the value.
                        let cleaned = text.replace(['\n', '\r'], "");
                        o.push_str(&cleaned);
                    }
                } else if self.first_run_wizard.is_none() {
                    // Terminals transmit pasted line breaks as CR, so this
                    // payload arrives as "a\rb" for a two-line paste. Land it
                    // in the buffer as '\n': that is the invariant the caret
                    // walker counts and the renderer splits on, and a raw '\r'
                    // reaching the terminal wrecks the input box's border.
                    self.context.paste_at_cursor(&normalize_line_breaks(&text));
                    self.context.exit_history_navigation();
                    self.refresh_autocomplete();
                }
                Ok(EventResult::Continue)
            }
            _ => Ok(EventResult::Continue),
        }
    }

    /// Dispatch a key event.
    pub async fn handle_key(&mut self, key: KeyEvent) -> Result<EventResult> {
        // Drain any pending ClawHub search results before processing the
        // next key — late-arriving results land in the picker before the
        // user's next action so they always see the freshest state.
        self.drain_clawhub_search_results();

        // Console login gate — owns the screen until the password verifies.
        // Intercept here (before every other handler) and return early so the
        // app behind it stays frozen; no need to guard each normal-key branch.
        if self.login_gate.is_some() {
            match key.code {
                KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.state = AppState::Quitting;
                    return Ok(EventResult::Quit);
                }
                KeyCode::Enter => {
                    let hash = self.config.gateway.login.password_hash.clone();
                    let ok = match (self.login_gate.as_ref(), hash.as_deref()) {
                        (Some(gate), Some(h)) => gate.check(h),
                        // Credential gone (login disabled mid-session) → let through.
                        _ => true,
                    };
                    if ok {
                        self.login_gate = None;
                    } else if let Some(gate) = self.login_gate.as_mut() {
                        gate.input.clear();
                        gate.error = Some("Incorrect password".to_string());
                    }
                }
                KeyCode::Backspace => {
                    if let Some(gate) = self.login_gate.as_mut() {
                        gate.input.pop();
                    }
                }
                KeyCode::Char(c)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    if let Some(gate) = self.login_gate.as_mut() {
                        gate.input.push(c);
                    }
                }
                _ => {}
            }
            return Ok(EventResult::Continue);
        }

        // Inline pending-approval prompt: when an approval is open and
        // the user hits Y/N/A/Esc, resolve it without going through
        // `/allow` slash commands. Modifiers must be empty so we don't
        // clash with Ctrl-Y (none today, future-proofing) or
        // accidentally fire when the user types "yes" into the buffer.
        // This arm runs BEFORE the Ctrl+D/Ctrl+C globals so the prompt
        // wins for these specific keys; quit shortcuts still work for
        // everything else.
        if self.pending_approval.is_some() && key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.resolve_pending_approval(crate::security::Decision::Session);
                    return Ok(EventResult::Continue);
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.resolve_pending_approval(crate::security::Decision::Persist);
                    return Ok(EventResult::Continue);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.resolve_pending_approval(crate::security::Decision::Deny);
                    return Ok(EventResult::Continue);
                }
                _ => {} // fall through to normal key handling
            }
        }

        match key.code {
            // Ctrl+D → always quit
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.state = AppState::Quitting;
                return Ok(EventResult::Quit);
            }
            // Ctrl+C → cancel streaming turn if active; otherwise quit
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                match &mut self.state {
                    AppState::Streaming { cancelling, .. } => {
                        *cancelling = true;
                        if let Err(e) = self.context.req_tx.send(TurnRequest::Cancel).await {
                            // Bridge closed — fall through to quit.
                            self.context.last_error = Some(format!("cancel failed: {e}"));
                            self.state = AppState::Quitting;
                            return Ok(EventResult::Quit);
                        }
                        return Ok(EventResult::Continue);
                    }
                    AppState::Ready | AppState::Quitting => {
                        self.state = AppState::Quitting;
                        return Ok(EventResult::Quit);
                    }
                }
            }
            // List picker active — intercepts arrows/enter/esc and
            // routes printable chars to the search query. Up/Down move
            // within the *filtered* view; Enter dispatches the selected
            // item by kind; Esc dismisses; Backspace deletes from the
            // query. All other keys are swallowed so the user can't
            // type into the input buffer behind the picker.
            KeyCode::Up if self.list_picker.is_some() => {
                if let Some(p) = self.list_picker.as_mut() {
                    p.move_up();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Down if self.list_picker.is_some() => {
                if let Some(p) = self.list_picker.as_mut() {
                    p.move_down();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Left if self.list_picker.is_some() => {
                if let Some(p) = self.list_picker.as_mut() {
                    p.prev_page();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Right if self.list_picker.is_some() => {
                if let Some(p) = self.list_picker.as_mut() {
                    p.next_page();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Enter if self.list_picker.is_some() => {
                use super::widgets::list_picker::Focus;
                use super::widgets::ListPickerKind;
                let (kind, focus, query) = match self.list_picker.as_ref() {
                    Some(p) => (p.kind, p.focus, p.query.clone()),
                    None => return Ok(EventResult::Continue),
                };
                // ClawhubInstall picker has a two-mode Enter:
                //   1. Focus::Search → fire a fresh ClawHub search.
                //      Empty query falls back to top-by-stars listing.
                //      Picker stays open.
                //   2. Focus::List → install the highlighted skill. Picker
                //      stays open afterwards so the user can search again
                //      and install more without re-running the command.
                if kind == ListPickerKind::ClawhubInstall {
                    if focus == Focus::Search {
                        self.spawn_clawhub_search(&query);
                        // After firing, move focus to the list so the
                        // next Enter installs (when results arrive).
                        if let Some(p) = self.list_picker.as_mut() {
                            p.focus = Focus::List;
                        }
                        return Ok(EventResult::Continue);
                    }
                    let slug = match self
                        .list_picker
                        .as_ref()
                        .and_then(|p| p.current().map(|i| i.key.clone()))
                    {
                        Some(s) => s,
                        None => return Ok(EventResult::Continue),
                    };
                    if self.clawhub_install_in_progress.is_some() {
                        // Already installing — ignore the second Enter so
                        // we don't fire two parallel installs.
                        return Ok(EventResult::Continue);
                    }
                    // Spawn install in a background tokio task and return
                    // immediately. The render loop's tick handler polls
                    // `clawhub_install_completion_rx` each frame, advances
                    // the spinner animation, and swaps the picker when the
                    // install finishes.  Pre-fix code awaited install
                    // inline, which froze the entire TUI for the whole
                    // network round-trip — no spinner, no animation, just
                    // instant flicker into the next picker.
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<String, String>>();
                    self.clawhub_install_completion_rx = Some(rx);
                    self.clawhub_install_completion_tx = Some(tx.clone());
                    self.clawhub_install_in_progress = Some((slug.clone(), 0));
                    let profile = self.profile.clone();
                    let slug_for_task = slug.clone();
                    tokio::spawn(async move {
                        let send =
                            match crate::skills::clawhub::install_one(&profile, &slug_for_task)
                                .await
                            {
                                Ok(()) => Ok(slug_for_task),
                                Err(e) => Err(format!("{e:#}")),
                            };
                        let _ = tx.send(send);
                    });
                    return Ok(EventResult::Continue);
                }
                self.dispatch_list_picker_selection().await;
                return Ok(EventResult::Continue);
            }
            KeyCode::Char('i')
                if self.list_picker.as_ref().is_some_and(|p| {
                    p.kind == crate::tui::widgets::ListPickerKind::Skill
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                }) =>
            {
                self.spawn_skill_deps_install();
                return Ok(EventResult::Continue);
            }
            KeyCode::Tab
                if self
                    .list_picker
                    .as_ref()
                    .is_some_and(|p| p.kind == crate::tui::widgets::ListPickerKind::Skill) =>
            {
                self.spawn_skill_deps_install();
                return Ok(EventResult::Continue);
            }
            KeyCode::Esc if self.list_picker.is_some() => {
                self.list_picker = None;
                self.close_clawhub_install_picker_state();
                self.close_skill_deps_install_state();
                return Ok(EventResult::Continue);
            }
            KeyCode::Backspace if self.list_picker.is_some() => {
                if let Some(p) = self.list_picker.as_mut() {
                    p.pop_query_char();
                    // Mark a search as pending for the ClawHub picker so
                    // the "↵ Enter to search ClawHub" hint shows up next
                    // to the typed query — pre-fix users typed and saw
                    // nothing happen, since search only fires on Enter.
                    if p.kind == crate::tui::widgets::ListPickerKind::ClawhubInstall {
                        p.search_pending = !p.query.is_empty();
                    }
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Char(c)
                if self.list_picker.is_some()
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(p) = self.list_picker.as_mut() {
                    p.push_query_char(c);
                    if p.kind == crate::tui::widgets::ListPickerKind::ClawhubInstall {
                        p.search_pending = true;
                    }
                }
                return Ok(EventResult::Continue);
            }
            _ if self.list_picker.is_some() => {
                // Picker open — swallow everything else.
                return Ok(EventResult::Continue);
            }
            // Info panel active — read-only modal, so the keymap is just
            // scroll + close. Mirrors the list_picker pattern so the user
            // doesn't have to learn two modal-key dialects.
            KeyCode::Up if self.info_panel.is_some() => {
                if let Some(p) = self.info_panel.as_mut() {
                    p.scroll_up();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Down if self.info_panel.is_some() => {
                if let Some(p) = self.info_panel.as_mut() {
                    p.scroll_down();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::PageUp if self.info_panel.is_some() => {
                if let Some(p) = self.info_panel.as_mut() {
                    p.page_up();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::PageDown if self.info_panel.is_some() => {
                if let Some(p) = self.info_panel.as_mut() {
                    p.page_down();
                }
                return Ok(EventResult::Continue);
            }
            KeyCode::Esc if self.info_panel.is_some() => {
                self.info_panel = None;
                return Ok(EventResult::Continue);
            }
            _ if self.info_panel.is_some() => {
                // Info panel open — swallow everything else (read-only).
                return Ok(EventResult::Continue);
            }
            // Tab — completes the highlighted command from the dropdown.
            // Always wins over the cycling-tabs handler when typing a
            // slash command (the autocomplete is visible only then).
            KeyCode::Tab if self.autocomplete.is_visible() => {
                self.complete_selected_command();
            }
            // Shift+Tab — Claude-Code-style cycle through approval-policy
            // presets (Manual → Smart → Strict → Off → …). Gated on no
            // modal active: list_picker / info_panel return early above,
            // but setup_overlay and first_run_wizard own the screen at
            // their own arms further down. Cycling autonomy from inside
            // the setup wizard is a silent state change that the user
            // didn't ask for.
            KeyCode::BackTab if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                self.cycle_autonomy_preset();
                return Ok(EventResult::Continue);
            }
            // Ctrl+Enter → submit (Kitty-protocol terminals).
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.autocomplete.hide();
                self.submit_input().await?;
            }
            // Ctrl+J → newline (alt for terminals that don't pass Shift+Enter).
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.context.insert_char_at_cursor('\n');
            }
            // Ctrl+G → suspend the TUI and open the current input
            // buffer in $EDITOR. The actual swap happens in `run_loop`
            // (which owns the Terminal); we just raise a flag here.
            // Gated on no modal active — the external editor expects to
            // edit the chat composer's input_buffer, not whatever the
            // setup wizard or first-run wizard is collecting.
            KeyCode::Char('g')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none() =>
            {
                self.editor_request = true;
            }
            // Ctrl+B — back. While the first-run wizard is open, walk the
            // phase history one step back. If a provisioner is currently
            // running, first send Cancelled so the task exits cleanly and
            // tear down the overlay state, then walk history (which skips
            // RunningProvisioner entries because the task already wrote to
            // config — rewinding mid-task isn't safe). Tester ask:
            // "setup should can have back mechanism" (bugs-123).
            KeyCode::Char('b')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.first_run_wizard.is_some() =>
            {
                let was_running_provisioner = self
                    .first_run_wizard
                    .as_ref()
                    .is_some_and(|w| w.is_provisioner_running());
                if was_running_provisioner {
                    let was_finished = self
                        .setup_overlay
                        .as_ref()
                        .map(|o| o.finished)
                        .unwrap_or(false);
                    if !was_finished {
                        if let Some(tx) = self.setup_response_tx.take() {
                            let _ = tx
                                .send(crate::onboard::provision::ProvisionResponse::Cancelled)
                                .await;
                        }
                    }
                    self.setup_overlay = None;
                    self.setup_event_rx = None;
                    self.setup_save_complete_rx = None;
                    self.setup_response_tx = None;
                }
                if let Some(w) = self.first_run_wizard.as_mut() {
                    if w.back() {
                        // History pop succeeded — clear any stale picker
                        // state so the destination phase re-initializes
                        // cleanly on next render.
                        w.picker = None;
                        w.picker_names.clear();
                    }
                }
            }
            // Plain Enter — Hermes / Claude Code convention:
            //   * If the autocomplete dropdown is visible and the highlighted
            //     command differs from what the user typed → complete first;
            //     a second Enter then fires it. If it already matches, submit.
            //   * Otherwise → submit. Period. Both prose and slash commands.
            //
            // Multi-line prompts work via Ctrl+J (handled above) or
            // Shift+Enter on terminals with the kitty keyboard protocol.
            KeyCode::Enter if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                if self.autocomplete.is_visible() {
                    let buf = self.context.input_buffer.trim_end_matches(' ').to_string();
                    let selected = self.autocomplete.selected().map(str::to_string);
                    if selected.as_deref() == Some(buf.as_str()) {
                        self.autocomplete.hide();
                        self.submit_input().await?;
                    } else {
                        self.complete_selected_command();
                    }
                } else {
                    self.submit_input().await?;
                }
            }
            // Escape — dismiss the dropdown without changing the buffer.
            KeyCode::Esc if self.autocomplete.is_visible() => {
                self.autocomplete.hide();
            }
            // Escape — close the modal overlay (e.g. /help).
            KeyCode::Esc if self.overlay.is_some() => {
                self.overlay = None;
            }
            // Tab cycles tabs in the overlay.
            KeyCode::Tab if self.overlay.is_some() => {
                if let Some(o) = self.overlay.as_mut() {
                    if !o.tabs.is_empty() {
                        o.active_tab = (o.active_tab + 1) % o.tabs.len();
                    }
                }
            }
            // Left/Right also cycle tabs in the overlay.
            KeyCode::Left if self.overlay.is_some() => {
                if let Some(o) = self.overlay.as_mut() {
                    if !o.tabs.is_empty() {
                        o.active_tab = if o.active_tab == 0 {
                            o.tabs.len() - 1
                        } else {
                            o.active_tab - 1
                        };
                    }
                }
            }
            KeyCode::Right if self.overlay.is_some() => {
                if let Some(o) = self.overlay.as_mut() {
                    if !o.tabs.is_empty() {
                        o.active_tab = (o.active_tab + 1) % o.tabs.len();
                    }
                }
            }
            // Backspace — delete the char immediately before the cursor.
            KeyCode::Backspace
                if self.setup_overlay.is_none() && self.first_run_wizard.is_none() =>
            {
                self.context.backspace_at_cursor();
                self.context.exit_history_navigation();
                self.refresh_autocomplete();
            }
            // Delete — remove the char at the cursor (cursor stays put).
            KeyCode::Delete if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                self.context.delete_at_cursor();
                self.context.exit_history_navigation();
                self.refresh_autocomplete();
            }
            // Regular character input — insert at the cursor.
            // Plain text input. The CONTROL check is load-bearing: crossterm
            // reports an unhandled chord like Ctrl+U as `Char('u')` with the
            // CONTROL modifier, so without it every readline chord the app
            // does not implement typed its own letter into the buffer
            // (Ctrl+A/E/W/K/U on `hello` produced `helloaewku`). Chords we do
            // implement — Ctrl+C/D/J/G — match in arms above this one. SHIFT
            // and ALT must still pass through: they carry real text.
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none() =>
            {
                self.context.insert_char_at_cursor(c);
                self.context.exit_history_navigation();
                self.refresh_autocomplete();
            }
            // Left/Right move the cursor inside the input buffer when no
            // overlay/picker has claimed them. Overlay tab navigation
            // arms below already gated on overlay presence, so they run
            // first and these arms only fire when nothing else needs the
            // arrow keys.
            KeyCode::Left
                if self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none()
                    && self.list_picker.is_none()
                    && self.info_panel.is_none()
                    && self.overlay.is_none() =>
            {
                self.context.cursor_left();
            }
            KeyCode::Right
                if self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none()
                    && self.list_picker.is_none()
                    && self.info_panel.is_none()
                    && self.overlay.is_none() =>
            {
                self.context.cursor_right();
            }
            // Home / End jump to the buffer extremes.
            KeyCode::Home if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                self.context.cursor_home();
            }
            KeyCode::End if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                self.context.cursor_end();
            }
            // Up/Down navigate the dropdown when visible; otherwise scroll
            // the chat history.
            KeyCode::Up if self.autocomplete.is_visible() => {
                self.autocomplete.previous();
            }
            KeyCode::Down if self.autocomplete.is_visible() => {
                self.autocomplete.next();
            }
            // Up/Down with no modal active recalls submitted prompts
            // from history. Native terminal scrollback (mouse wheel /
            // PgUp on the host terminal) handles chat scrolling.
            KeyCode::Up if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                if let Some(text) = self.context.history_recall_older() {
                    self.context.input_buffer = text;
                    self.context.cursor_to_end();
                    self.refresh_autocomplete();
                }
            }
            KeyCode::Down if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                if let Some(text) = self.context.history_recall_newer() {
                    self.context.input_buffer = text;
                    self.context.cursor_to_end();
                    self.refresh_autocomplete();
                }
            }
            // Esc — close the setup overlay or cancel the wizard.
            KeyCode::Esc if self.setup_overlay.is_some() || self.first_run_wizard.is_some() => {
                // If wizard is active, cancel the entire wizard.
                if self.first_run_wizard.is_some() {
                    let was_finished = self
                        .setup_overlay
                        .as_ref()
                        .map(|o| o.finished)
                        .unwrap_or(false);
                    if !was_finished {
                        if let Some(tx) = self.setup_response_tx.take() {
                            let _ = tx
                                .send(crate::onboard::provision::ProvisionResponse::Cancelled)
                                .await;
                        }
                    }
                    self.first_run_wizard = None;
                    self.setup_overlay = None;
                    self.setup_event_rx = None;
                    self.setup_save_complete_rx = None;
                    self.setup_response_tx = None;
                    if let Err(e) = self.reload_config() {
                        tracing::warn!("failed to reload config after wizard cancel: {}", e);
                    }
                    return Ok(EventResult::Continue);
                }
                let was_finished = self
                    .setup_overlay
                    .as_ref()
                    .map(|o| o.finished)
                    .unwrap_or(false);
                // Cancel only if the provisioner is still running. If
                // `finished` is set, the task has already exited and
                // sending Cancelled would be a no-op (and may panic
                // if the receiver has been dropped).
                if !was_finished {
                    if let Some(tx) = self.setup_response_tx.take() {
                        let _ = tx
                            .send(crate::onboard::provision::ProvisionResponse::Cancelled)
                            .await;
                    }
                }
                self.setup_overlay = None;
                self.setup_event_rx = None;
                self.setup_save_complete_rx = None;
                self.setup_response_tx = None;
                // Reload config so freshly-written sections take effect.
                if let Err(e) = self.reload_config() {
                    tracing::warn!("failed to reload config after setup: {}", e);
                }
            }
            // Esc — cancel the in-flight streaming turn. Mirrors Ctrl+C's
            // streaming branch but only acts during Streaming, never as a
            // quit (Ctrl+C still handles quit). This arm sits AFTER every
            // modal-specific Esc handler above so closing a picker /
            // overlay / wizard always wins over cancel; Esc only reaches
            // here when no modal is up and the agent is actively running.
            // Matches the working indicator's `esc to interrupt` hint.
            KeyCode::Esc if matches!(self.state, AppState::Streaming { .. }) => {
                if let AppState::Streaming { cancelling, .. } = &mut self.state {
                    *cancelling = true;
                }
                if let Err(e) = self.context.req_tx.send(TurnRequest::Cancel).await {
                    self.context.last_error = Some(format!("cancel failed: {e}"));
                }
                return Ok(EventResult::Continue);
            }
            // Enter — submit the active prompt response.
            KeyCode::Enter
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_prompt().is_some()) =>
            {
                let (_id, value) = {
                    let overlay = self.setup_overlay.as_mut().unwrap();
                    overlay
                        .submit_prompt()
                        .unwrap_or_else(|| (String::new(), String::new()))
                };
                if let Some(tx) = &self.setup_response_tx {
                    let _ = tx
                        .send(crate::onboard::provision::ProvisionResponse::Text(value))
                        .await;
                }
                // Also drain the event that carries the prompt to the overlay.
                if let Some(rx) = &mut self.setup_event_rx {
                    while let Ok(ev) = rx.try_recv() {
                        if let Some(o) = &mut self.setup_overlay {
                            o.handle_event(ev);
                        }
                    }
                }
            }
            // Char input — route to overlay input when a prompt is active.
            KeyCode::Char(c)
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_prompt().is_some()) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.push_char(c);
                }
            }
            // Backspace — delete from overlay input when a prompt is active.
            KeyCode::Backspace
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_prompt().is_some()) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.pop_char();
                }
            }
            // Up — choose navigation.
            KeyCode::Up
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_choose().is_some()) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.choose_move_up();
                }
            }
            // Down — choose navigation.
            KeyCode::Down
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_choose().is_some()) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.choose_move_down();
                }
            }
            // Space — toggle in multi-select choose.
            KeyCode::Char(' ')
                if self.setup_overlay.is_some()
                    && self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_choose().is_some()) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.choose_toggle();
                }
            }
            // First-run wizard Enter handling — non-provisioner phases only.
            // Wizard picker (PickChannels / PickIntegrations): route
            // navigation/toggle keys to the wizard's own multi-select
            // state. Enter is handled by the wizard Enter arm below.
            KeyCode::Up
                if self
                    .first_run_wizard
                    .as_ref()
                    .is_some_and(|w| w.is_picker_active()) =>
            {
                if let Some(w) = self.first_run_wizard.as_mut() {
                    w.picker_move_up();
                }
            }
            KeyCode::Down
                if self
                    .first_run_wizard
                    .as_ref()
                    .is_some_and(|w| w.is_picker_active()) =>
            {
                if let Some(w) = self.first_run_wizard.as_mut() {
                    w.picker_move_down();
                }
            }
            KeyCode::Char(' ')
                if self
                    .first_run_wizard
                    .as_ref()
                    .is_some_and(|w| w.is_picker_active()) =>
            {
                if let Some(w) = self.first_run_wizard.as_mut() {
                    w.picker_toggle();
                }
            }
            // Wizard Enter is only meaningful for non-running phases.
            // During RunningProvisioner, Enter belongs to the active
            // setup_overlay (Choose submit / Prompt submit) — yield via
            // the guard so the overlay-Enter arms below can match.
            KeyCode::Enter
                if self.first_run_wizard.is_some()
                    && !matches!(
                        self.first_run_wizard.as_ref().unwrap().phase,
                        super::first_run_wizard::WizardPhase::RunningProvisioner { .. }
                    ) =>
            {
                let wizard = self.first_run_wizard.as_mut().unwrap();
                match wizard.phase.clone() {
                    super::first_run_wizard::WizardPhase::Welcome => {
                        wizard.start_provisioners();
                        self.react_to_wizard_phase();
                    }
                    super::first_run_wizard::WizardPhase::PickChannels
                    | super::first_run_wizard::WizardPhase::PickIntegrations => {
                        // Submit the multi-select; map indices to names;
                        // queue selected provisioners; advance.
                        wizard.apply_picker_selection();
                        self.react_to_wizard_phase();
                    }
                    super::first_run_wizard::WizardPhase::Complete => {
                        // Reload config so the freshly-saved provisioner
                        // sections take effect in the running TUI.
                        if let Err(e) = self.reload_config() {
                            tracing::warn!("failed to reload config after wizard: {}", e);
                        }
                        self.first_run_wizard = None;
                    }
                    super::first_run_wizard::WizardPhase::RunningProvisioner { .. } => {
                        // Unreachable due to guard; kept for exhaustiveness.
                    }
                }
            }
            // Enter — submit choose or submit prompt. Skip if wizard is in Welcome
            // phase (the wizard Enter handler takes over in that case).
            KeyCode::Enter
                if self.setup_overlay.is_some()
                    && !self.first_run_wizard.as_ref().is_some_and(|w| {
                        matches!(w.phase, super::first_run_wizard::WizardPhase::Welcome)
                    })
                    && (self
                        .setup_overlay
                        .as_ref()
                        .is_some_and(|o| o.active_choose().is_some())
                        || self
                            .setup_overlay
                            .as_ref()
                            .is_some_and(|o| o.active_prompt().is_some())) =>
            {
                // Submit (and clear) choose/prompt state, then forward
                // the response to the provisioner. Awaited so the
                // future actually runs — without `.await`, the send
                // future is dropped, the provisioner blocks on
                // recv_selection/recv_text forever, and the user is
                // left staring at an empty overlay with no way to
                // advance.
                let outgoing = self.setup_overlay.as_mut().and_then(|o| {
                    if let Some((_id, sel)) = o.submit_choose() {
                        Some(crate::onboard::provision::ProvisionResponse::Selection(sel))
                    } else {
                        o.submit_prompt().map(|(_id, val)| {
                            crate::onboard::provision::ProvisionResponse::Text(val)
                        })
                    }
                });
                if let (Some(resp), Some(tx)) = (outgoing, self.setup_response_tx.as_ref()) {
                    let _ = tx.send(resp).await;
                }
            }
            // Setup overlay open with no active prompt/choose — Up/Down/
            // PageUp/PageDown/Home/End scroll the overlay so the user can
            // see content (especially the QR + status log) that exceeds
            // the terminal height.
            KeyCode::Up if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.scroll_up();
                }
            }
            KeyCode::Down if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.scroll_down();
                }
            }
            // PageUp/PageDown/Home/End — when the choose picker is active,
            // route to picker navigation (jump cursor a page at a time);
            // otherwise scroll the log panel. Lets the user fly through
            // long lists like ClawHub's 20-skill picker without one ↓ at
            // a time. Tester ask: "no good pagination, user can't scroll".
            KeyCode::PageUp if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    if o.active_choose().is_some() {
                        o.choose_page_up();
                    } else {
                        o.scroll_page_up();
                    }
                }
            }
            KeyCode::PageDown if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    if o.active_choose().is_some() {
                        o.choose_page_down();
                    } else {
                        o.scroll_page_down();
                    }
                }
            }
            KeyCode::Home if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    if o.active_choose().is_some() {
                        o.choose_home();
                    } else {
                        o.scroll_home();
                    }
                }
            }
            KeyCode::End if self.setup_overlay.is_some() => {
                if let Some(o) = self.setup_overlay.as_mut() {
                    if o.active_choose().is_some() {
                        o.choose_end();
                    } else {
                        o.scroll_end();
                    }
                }
            }
            // Catch-all for any other key while overlay is open — swallow.
            _ if self.setup_overlay.is_some() => {
                return Ok(EventResult::Continue);
            }
            _ => {}
        }
        Ok(EventResult::Continue)
    }

    /// Submit the current input buffer as a message (or command).
    ///
    /// For slash-prefixed input, dispatches to the command registry.
    /// For message input in `Ready` state, records the user turn, sends a
    /// `TurnRequest::Submit` to the `TuiAgentActor` via the bridge, and
    /// transitions to `Streaming`. For message input in `Streaming` state,
    /// the request is still dispatched (the actor will queue it) and
    /// `queued_turns` is incremented so the status bar reflects backlog.
    pub async fn submit_input(&mut self) -> Result<()> {
        let raw = std::mem::take(&mut self.context.input_buffer);
        self.context.cursor_pos = 0;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        // Record this submission in input history so Up/Down can
        // recall it later. Also reset history navigation state so the
        // next Up press starts from the most recent entry.
        self.context.push_input_history(trimmed);
        self.context.exit_history_navigation();

        // Slash-commands bypass the bridge entirely (except `/retry`
        // which dispatches via `handle_command` → `dispatch_resubmit`).
        if let Some(cmd) = trimmed.strip_prefix('/') {
            // Echo the slash command into scrollback first, so the user
            // can see what they typed above the response (matches the
            // normal user/assistant exchange flow).
            self.scrollback_queue
                .push(("user".to_string(), format!("/{cmd}", cmd = cmd.trim())));
            let cmd = cmd.trim().to_string();
            self.handle_command(&cmd).await?;
            self.context.scroll_offset = 0;
            return Ok(());
        }

        let text = trimmed.to_string();
        self.context.append_user_message(&text)?;
        // Commit user message to scrollback so it appears inline like
        // Hermes/Claude-Code chat output.
        self.scrollback_queue
            .push(("user".to_string(), text.clone()));

        // Dispatch to the actor. If the bridge is closed (e.g. actor has
        // exited), surface a visible error but do not propagate — the TUI
        // should remain responsive so the user can /quit cleanly.
        if let Err(e) = self.context.req_tx.send(TurnRequest::Submit(text)).await {
            self.context.last_error = Some(format!("agent bridge closed: {e}"));
            self.context.scroll_offset = 0;
            return Ok(());
        }

        match self.state {
            AppState::Ready => {
                self.state = AppState::Streaming {
                    partial: String::new(),
                    tool_blocks: Vec::new(),
                    cancelling: false,
                    turn_started_at: std::time::Instant::now(),
                };
                self.stream_committed_chars = 0;
                self.stream_header_committed = false;
            }
            AppState::Streaming { .. } => {
                self.context.queued_turns += 1;
            }
            AppState::Quitting => {}
        }

        self.context.scroll_offset = 0;
        Ok(())
    }

    /// Drain any queued `AgentEvent`s from the bridge without blocking.
    ///
    /// Called once per frame by the render loop, before rendering, so that
    /// state transitions (Chunk, ToolCall*, Usage, Done, Error) are reflected
    /// in the next paint. Uses `try_recv` to remain non-blocking on an empty
    /// channel; a closed channel is treated the same as empty here — the
    /// actor lifecycle is separately managed by `run_tui`.
    pub fn drain_events(&mut self) {
        while let Ok(ev) = self.context.events_rx.try_recv() {
            self.handle_agent_event(ev);
        }

        // Pending-approval requests from the shell tool. Each new
        // request surfaces as a system message with the exact slash
        // commands the user can type to resolve it.
        if let Some(rx) = self.pending_approvals_rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(req) => {
                        // Short audit-trail line for scrollback — the
                        // real prompt UI is the boxed widget that takes
                        // over the input row. The full command was
                        // already echoed in the assistant's tool block
                        // line, so this is just the gate fingerprint.
                        let line = format!(
                            "🔒 awaiting decision on `{}` (press Y/A/N or Esc)",
                            req.basename
                        );
                        let _ = self.context.append_system_message(&line);
                        self.scrollback_queue.push(("system".into(), line));
                        // Stash the latest pending request so the key
                        // handler can resolve it inline via single
                        // keystroke without the user typing /allow X.
                        // Replacing an earlier unresolved request is
                        // fine — the inner registry tracks all of them
                        // by id, and the user only sees the newest in
                        // the prompt. Older ones still auto-deny on
                        // timeout.
                        self.pending_approval = Some(req);
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                        // Missed some; user can /allowlist to see what's still pending.
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        self.pending_approvals_rx = None;
                        break;
                    }
                }
            }
        }
        if let Some(rx) = &mut self.setup_event_rx {
            // Two-phase drain: first sweep events into a buffer so we can
            // intercept OpenSkillInstallPicker (which the overlay shouldn't
            // see — it hands off to the install picker, not the choose
            // render). Other events forward to setup_overlay as before.
            let mut buffered = Vec::new();
            while let Ok(ev) = rx.try_recv() {
                buffered.push(ev);
            }
            for ev in buffered {
                match ev {
                    crate::onboard::provision::ProvisionEvent::OpenSkillInstallPicker {
                        ..
                    } => {
                        // Hand off to the live install picker. We track that
                        // we're in "wizard install" mode so the picker's
                        // close path knows to send InstalledSkills back to
                        // the wizard's response channel.
                        self.wizard_install_in_progress = true;
                        self.wizard_installed_slugs.clear();
                        // Fire-and-forget: open_clawhub_install_picker is
                        // async, but we're inside drain_events (sync). Use
                        // a tiny future-now: spawn the prep that doesn't
                        // need awaiting (state init), defer the network
                        // fetch to drain_clawhub_search_results.
                        self.open_clawhub_install_picker_sync(None);
                    }
                    other => {
                        if let Some(overlay) = &mut self.setup_overlay {
                            overlay.handle_event(other);
                        }
                    }
                }
            }
        }
        self.drain_skill_reload_events();
        self.drain_config_reload_events();
        // ClawHub install picker — async search results stream in via a
        // background task. Drain on each render tick so newly-arrived
        // results land between user actions, not just after the next key.
        self.drain_clawhub_search_results();
        // Same idea for install completion + spinner animation: advance
        // the spinner frame and check whether the spawned install task
        // has produced a result.
        self.tick_clawhub_install();
        self.tick_skill_deps_install();
        // Wizard auto-advance: when the current provisioner finishes
        // SUCCESSFULLY AND its config.save() has completed, close the
        // overlay and open the next provisioner (or advance to Complete
        // if this was the last one). On failure, keep the overlay open
        // so the user reads the error and decides what to do (Esc
        // aborts the wizard).
        //
        // The save-completion gate (v0.6.56) closes a race where the
        // provisioner emitted `Done` via the events channel BEFORE its
        // async `config.save().await` finished — the wizard would
        // advance, clone a stale `self.config`, run the next
        // provisioner, and that next save would trample the previous
        // one. Net effect: only the last provisioner's writes landed
        // on disk; provider/api_key/etc were wiped. We now poll the
        // setup_save_complete_rx oneshot and only advance when the
        // saved Config has been received (and swapped into self.config)
        // so the next clone sees the latest writes.
        let saved_config = match self.setup_save_complete_rx.as_mut() {
            Some(rx) => match rx.try_recv() {
                Ok(cfg) => Some(Some(cfg)), // Some(Some) = save succeeded, here's the config
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => Some(None), // Some(None) = save failed / spawn aborted
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None, // still pending
            },
            None => None,
        };
        if let Some(Some(cfg)) = &saved_config {
            // Save succeeded — swap in the new config NOW so the next
            // provisioner's clone sees this provisioner's writes.
            self.config = cfg.clone();
        }
        if saved_config.is_some() {
            // Either Ok or Err — the oneshot is consumed; drop the rx.
            self.setup_save_complete_rx = None;
        }
        let need_advance = self
            .first_run_wizard
            .as_ref()
            .is_some_and(|w| w.is_provisioner_running())
            && {
                let o = self.setup_overlay.as_ref();
                o.is_some_and(|s| s.finished) && o.is_some_and(|s| s.failure_reason.is_none())
            }
            // Gate: only advance once the save has completed (Some(Some))
            // OR if the spawn already errored out (Some(None) — handled
            // via overlay.failure_reason elsewhere, but defensively we
            // refuse to advance on a closed save channel since it means
            // nothing was persisted).
            && matches!(saved_config, Some(Some(_)));
        if need_advance {
            // Clean success → close overlay, advance wizard, react.
            self.setup_overlay = None;
            self.setup_event_rx = None;
            self.setup_save_complete_rx = None;
            self.setup_response_tx = None;
            if let Some(wizard) = self.first_run_wizard.as_mut() {
                wizard.advance_to_next_in_queue_or_picker();
            }
            self.react_to_wizard_phase();
        }
        // Failed: do nothing — overlay stays open showing the error;
        // user dismisses with Esc which exits the wizard.
        // Note: we no longer auto-clear the overlay when the provisioner
        // sets `finished = true`. The user dismisses via Esc so they can
        // read the success/failure summary at their own pace. The Esc
        // handler does the cleanup (clear overlay state + reload config).
    }

    /// Inspect the wizard's current phase and take the side-effect that
    /// matches it — open a setup overlay for a queued provisioner,
    /// open the multi-select picker for PickChannels / PickIntegrations,
    /// or do nothing for Welcome / Complete (handled by the Enter
    /// handler) or RunningProvisioner without a queued name yet.
    /// Called after every wizard transition.
    fn react_to_wizard_phase(&mut self) {
        let phase = match self.first_run_wizard.as_ref() {
            Some(w) => w.phase.clone(),
            None => return,
        };
        match phase {
            super::first_run_wizard::WizardPhase::RunningProvisioner { name } => {
                if let Err(e) = self.open_setup_overlay(name.clone()) {
                    let msg = format!("Failed to open provisioner '{name}': {e}");
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".to_string(), msg));
                }
            }
            super::first_run_wizard::WizardPhase::PickChannels => {
                if let Some(w) = self.first_run_wizard.as_mut() {
                    w.open_picker(super::first_run_wizard::channel_options());
                }
            }
            super::first_run_wizard::WizardPhase::PickIntegrations => {
                if let Some(w) = self.first_run_wizard.as_mut() {
                    w.open_picker(super::first_run_wizard::integration_options());
                }
            }
            super::first_run_wizard::WizardPhase::Welcome
            | super::first_run_wizard::WizardPhase::Complete => {}
        }
    }

    /// (Re)spawn the background channel runtime from the current
    /// `self.config`, tearing down any previously-running runtime first.
    ///
    /// This is the single mechanism behind two behaviours that previously
    /// required a full TUI restart:
    ///   * a channel configured mid-session (`/setup telegram`) starts
    ///     polling immediately, and
    ///   * a skill added mid-session is reflected in the channel runtime's
    ///     system prompt (skills are baked into the prompt at
    ///     `start_channels` build time, so the only way to pick up new
    ///     skills is to rebuild the runtime).
    ///
    /// The cancel → drain → start sequence runs inside one spawned task so
    /// the UI never blocks: the old runtime's listeners are cancelled and
    /// awaited (bounded) before the new ones bind the same backend,
    /// avoiding Telegram's single-consumer 409 window.
    fn restart_channels(&mut self) {
        let prev = self.channel_supervisor.take();
        let configured = count_configured_channels(&self.config);
        self.context.channels_autostart_count = configured;

        if configured == 0 {
            // Nothing to run now — just stop whatever was running (e.g. the
            // user removed their only channel mid-session).
            if let Some(prev) = prev {
                prev.shutdown.cancel();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(10), prev.handle).await;
                });
            }
            return;
        }

        // Single-runner guard: if a live daemon already owns the channels for
        // this profile, don't start a competing runtime in the TUI. Two
        // processes on one channel cause duplicate/contradictory replies
        // (WhatsApp) or `409 Conflict` poll flapping (Telegram). Defer to the
        // daemon; `rantaiclaw service restart` applies config changes.
        if let Some(pid) = crate::profile::sentinel::active_daemon_pid(&self.profile.name) {
            tracing::info!(
                "channels are managed by the running daemon (PID {pid}); not starting a duplicate in the TUI. Use `rantaiclaw service restart` to apply changes."
            );
            if let Some(prev) = prev {
                prev.shutdown.cancel();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(10), prev.handle).await;
                });
            }
            return;
        }

        let cfg = self.config.clone();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task_token = shutdown.clone();
        let handle = tokio::spawn(async move {
            // Drain the previous runtime before starting the new one so the
            // old and new listeners don't fight over the same backend
            // (Telegram getUpdates returns 409 to a second concurrent
            // poller). Cancel + bounded await guarantees the old long-poll
            // has released first.
            if let Some(prev) = prev {
                prev.shutdown.cancel();
                let _ = tokio::time::timeout(Duration::from_secs(10), prev.handle).await;
            }
            crate::channels::auto_start_state::mark_starting();
            match crate::channels::start_channels_with_cancellation(cfg, task_token).await {
                Ok(()) => {
                    crate::channels::auto_start_state::mark_terminated();
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    tracing::warn!(
                        "channel runtime (re)start failed (TUI continues; channels will not respond until fixed): {msg}"
                    );
                    crate::channels::auto_start_state::mark_failed(msg);
                }
            }
        });
        self.channel_supervisor = Some(ChannelSupervisor { shutdown, handle });
    }

    fn reload_config(&mut self) -> anyhow::Result<()> {
        let path = self.profile.config_toml();
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read config from {}", path.display()))?;
        let mut config: crate::config::Config =
            toml::from_str(&contents).context("failed to parse config file")?;
        // `config_path` is `#[serde(skip)]` so deserialization leaves
        // it as PathBuf::default(). Restore from the path we just
        // loaded — without this, the next config.save() bails with
        // "Config path must have a parent directory".
        config.config_path = path.clone();
        // `workspace_dir` is also `#[serde(skip)]`. Forgetting to
        // restore it after a reload sets `SecurityPolicy.workspace_dir`
        // to an empty path, which makes `tokio::process::Command::
        // current_dir("")` fail with "No such file or directory" at
        // spawn time — that's the "shell broken after config reload"
        // bug. Re-derive it from the active profile so the rebuilt
        // agent runs shell tools in the right cwd.
        config.workspace_dir = self.profile.workspace_dir();
        // Decrypt secrets before pushing to the agent. Without this the
        // agent receives encrypted blobs in `config.api_key` and friends
        // and every API call returns 401 "Missing Authentication header"
        // because the request builder rejects the malformed header. This
        // mirrors the decrypt pass that `Config::load_or_init` runs at
        // startup; without it, `/setup provider` saves a fresh key and
        // the running TUI immediately fails to use it.
        let rantaiclaw_dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let store = crate::security::SecretStore::new(&rantaiclaw_dir, config.secrets.encrypt);
        crate::config::schema::decrypt_optional_secret(
            &store,
            &mut config.api_key,
            "config.api_key",
        )?;
        crate::config::schema::decrypt_optional_secret(
            &store,
            &mut config.composio.api_key,
            "config.composio.api_key",
        )?;
        crate::config::schema::decrypt_optional_secret(
            &store,
            &mut config.browser.computer_use.api_key,
            "config.browser.computer_use.api_key",
        )?;
        crate::config::schema::decrypt_optional_secret(
            &store,
            &mut config.web_search.brave_api_key,
            "config.web_search.brave_api_key",
        )?;
        crate::config::schema::decrypt_optional_secret(
            &store,
            &mut config.storage.provider.config.db_url,
            "config.storage.provider.config.db_url",
        )?;
        for agent in config.agents.values_mut() {
            crate::config::schema::decrypt_optional_secret(
                &store,
                &mut agent.api_key,
                "config.agents.*.api_key",
            )?;
        }
        // Knowledge Base keys are encrypted at rest like `api_key`; decrypt
        // them here too so a wizard/`/setup knowledge` run leaves the running
        // agent with usable KB credentials instead of a raw `enc2:` blob
        // (mirrors the decrypt pass in `Config::load_or_init`).
        crate::config::schema::decrypt_optional_secret(
            &store,
            &mut config.knowledge.embedding_api_key,
            "config.knowledge.embedding_api_key",
        )?;
        crate::config::schema::decrypt_optional_secret(
            &store,
            &mut config.knowledge.vision_api_key,
            "config.knowledge.vision_api_key",
        )?;
        config.apply_env_overrides();
        // Refresh the status-bar model label so the running TUI shows
        // the freshly-saved provider/model. Without this, a wizard run
        // that switches provider (e.g. openrouter → minimax) would
        // leave the status bar showing the old provider until the
        // next launch.
        let provider = config.default_provider.clone().unwrap_or_default();
        let model = config.default_model.clone().unwrap_or_default();
        let model_label = if !provider.is_empty() && !model.is_empty() {
            format!("{provider}:{model}")
        } else if !model.is_empty() {
            model
        } else {
            self.context.model.clone()
        };
        if !model_label.is_empty() {
            self.context.model = model_label;
        }
        // Recompute the /model picker's available-providers list from
        // the new config. Same logic as the startup-time computation
        // in run_tui — without this, /model still shows the old
        // provider's models after the wizard switched providers.
        let mut available_providers: Vec<String> = Vec::new();
        if let Some(p) = config.default_provider.clone() {
            available_providers.push(p);
        }
        for route in &config.model_routes {
            if !available_providers.iter().any(|p| p == &route.provider) {
                available_providers.push(route.provider.clone());
            }
        }
        self.context.available_providers = available_providers;
        // Refresh the channels snapshot so /channels and /platforms reflect
        // any wizard-driven add/remove since launch.
        let prev_channels_count = count_configured_channels(&self.config);
        self.context.channels_summary = channel_status_summary(&config)
            .into_iter()
            .map(|(name, configured)| (name.to_string(), configured))
            .collect();
        let new_channels_count = count_configured_channels(&config);
        self.context.channels_autostart_count = new_channels_count;
        // When the configured channel set changes mid-session (e.g.
        // `/setup telegram`), restart the background channel runtime in
        // place so newly-added channels start polling — and removed ones
        // stop — without closing the TUI. The restart itself runs after
        // `self.config` is updated below (it reads `self.config`); surface a
        // short status line so the user knows it's taking effect live.
        let channels_changed = new_channels_count != prev_channels_count;
        if channels_changed {
            let msg = if new_channels_count > prev_channels_count {
                format!(
                    "✓ {} new channel(s) configured — starting listener(s) now. \
                     `/channels` shows the current state.",
                    new_channels_count - prev_channels_count
                )
            } else {
                format!(
                    "✓ {} channel(s) removed — stopping their listener(s) now.",
                    prev_channels_count - new_channels_count
                )
            };
            let _ = self.context.append_system_message(&msg);
            self.scrollback_queue.push(("system".to_string(), msg));
        }
        self.config = config.clone();
        if channels_changed {
            self.restart_channels();
        }
        // Push the new config to the agent actor so the next turn uses
        // the freshly-saved provider/api_key/model. Without this the
        // agent stays pinned to the launch-time config and reports
        // "Missing API key" even though the wizard saved one.
        let req_tx = self.context.req_tx.clone();
        tokio::spawn(async move {
            let _ = req_tx
                .send(crate::tui::TurnRequest::Reload(Box::new(config)))
                .await;
        });
        // Config reload means the on-disk state changed (wizard saved,
        // /setup overlay closed, external editor wrote the file). Any
        // `last_error` from a previous turn — typically "missing API
        // key" / "model unavailable" — may now be resolved. Clear it
        // optimistically; the next failed turn will set a fresh error
        // through `finalize_error`, so we never leave the user staring
        // at a stale warning that doesn't match the current config.
        self.context.last_error = None;
        Ok(())
    }

    /// Apply a single `AgentEvent` to the app state.
    fn handle_agent_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Chunk(s) => {
                if let AppState::Streaming { partial, .. } = &mut self.state {
                    partial.push_str(&s);
                }
            }
            AgentEvent::ToolCallStart { id, name, args } => {
                if let AppState::Streaming { tool_blocks, .. } = &mut self.state {
                    tool_blocks.push(ToolBlockState {
                        id,
                        name,
                        args,
                        result: None,
                        started_at: std::time::Instant::now(),
                    });
                }
            }
            AgentEvent::ToolCallEnd {
                id,
                ok,
                output_preview,
            } => {
                // Pull the matching block, finalize its result, and
                // capture the fields we need for the scrollback line
                // before letting the borrow drop.
                let summary = if let AppState::Streaming { tool_blocks, .. } = &mut self.state {
                    if let Some(b) = tool_blocks.iter_mut().find(|b| b.id == id) {
                        b.result = Some((ok, output_preview.clone()));
                        let elapsed = b.started_at.elapsed();
                        Some((b.name.clone(), b.args.clone(), elapsed))
                    } else {
                        None
                    }
                } else {
                    None
                };

                // Inline-flush a 1-line summary to scrollback so the
                // user sees what the agent did *as it happens*. Format:
                //   ▸ shell(command="ls -la") → ok (12ms)
                //   ✗ shell(command="brew --version") → error (3s)
                if let Some((name, args, elapsed)) = summary {
                    let marker = if ok { "▸" } else { "✗" };
                    let args_compact = compact_args_for_log(&args);
                    let elapsed_label = if elapsed.as_secs() == 0 {
                        format!("{}ms", elapsed.as_millis())
                    } else {
                        format!("{}s", elapsed.as_secs())
                    };
                    let status = if ok {
                        let preview = output_preview.lines().next().unwrap_or("").trim();
                        if preview.is_empty() {
                            "ok".to_string()
                        } else if preview.len() > 60 {
                            format!("{}…", &preview[..60])
                        } else {
                            preview.to_string()
                        }
                    } else {
                        let preview = output_preview.lines().next().unwrap_or("").trim();
                        if preview.is_empty() {
                            "error".to_string()
                        } else if preview.len() > 60 {
                            format!("error: {}…", &preview[..60])
                        } else {
                            format!("error: {preview}")
                        }
                    };
                    let line =
                        format!("{marker} {name}({args_compact}) → {status} ({elapsed_label})");
                    self.scrollback_queue.push(("_tool_log".to_string(), line));
                }

                // UX nudge: if the agent is repeatedly hitting the
                // security gate on Smart/Manual/Strict, the user is
                // almost certainly trying a skill that needs broader
                // shell access. Surface a one-shot hint pointing at
                // `/autonomy off`. Threshold of 3 blocks per turn so
                // a single mistyped command doesn't trigger the toast.
                if !ok && output_preview.contains("Command not allowed by security policy") {
                    self.shell_blocks_this_turn = self.shell_blocks_this_turn.saturating_add(1);
                    if self.shell_blocks_this_turn == 3 && !self.autonomy_hint_shown_this_turn {
                        self.autonomy_hint_shown_this_turn = true;
                        let preset = self
                            .context
                            .autonomy_preset
                            .map(|p| p.label())
                            .unwrap_or("Smart");
                        let hint = format!(
                            "⚠ Multiple shell commands blocked by {preset} preset. \
                             If this is a trusted skill bootstrap, run /autonomy off \
                             (then /autonomy smart when done) for unrestricted access."
                        );
                        let _ = self.context.append_system_message(&hint);
                        self.scrollback_queue.push(("system".into(), hint));
                    }
                }
            }
            AgentEvent::Usage(u) => {
                // Map the agent's cost::TokenUsage onto the TUI's tally shape.
                self.context.token_usage = TokenUsage {
                    prompt_tokens: u.input_tokens,
                    completion_tokens: u.output_tokens,
                    total_tokens: u.total_tokens,
                };
            }
            AgentEvent::Done {
                final_text,
                cancelled,
            } => {
                self.finalize_turn(final_text, cancelled);
            }
            AgentEvent::Error(msg) => {
                self.finalize_error(msg);
            }
            AgentEvent::ReloadComplete {
                mcp_servers_configured,
                mcp_tools_by_server,
                security,
            } => {
                // Refresh the TUI's cached MCP snapshot so `/mcp`
                // reflects the post-reload state. The agent already
                // has the new tools and can use them on the next
                // turn — this just keeps the UI's view in sync.
                self.context.mcp_servers_configured = mcp_servers_configured.into_iter().collect();
                self.context.mcp_tools_by_server = mcp_tools_by_server;
                // Re-subscribe to the new SecurityPolicy's pending-
                // approval broadcast. The old subscriber was bound to
                // the previous registry, which the new shell tool
                // doesn't write to — without this, every approval
                // after the first /autonomy switch was silently
                // missed and the inline Y/N prompt never appeared.
                if let Some(new_security) = security {
                    if let Some(pending) = new_security.pending() {
                        self.pending_approvals_rx = Some(pending.subscribe());
                    }
                    self.context.security = Some(new_security);
                    // Drop any stale prompt left over from the
                    // previous registry — it's no longer resolvable.
                    self.pending_approval = None;
                }
                // Snapshot the freshly-active preset so the status bar
                // colour and inline command-allowlist hint match the
                // newly-loaded policy.
                self.context.autonomy_preset =
                    crate::approval::policy_writer::read_active_preset(&self.profile.policy_dir());
            }
            AgentEvent::CompactionStart {
                original_count,
                keep_last,
            } => {
                // Mirror the start of a streaming turn so the
                // working indicator runs and incoming Chunk events
                // are accumulated into `partial`. We don't queue a
                // request — the actor already invoked the agent —
                // so we just flip the state.
                self.state = AppState::Streaming {
                    partial: String::new(),
                    tool_blocks: Vec::new(),
                    turn_started_at: std::time::Instant::now(),
                    cancelling: false,
                };
                let _ = self.context.append_system_message(&format!(
                    "Compacting {original_count} message(s), preserving last \
                     {keep_last} turn(s) verbatim — summary streams below…"
                ));
            }
            AgentEvent::CompactionComplete {
                summary,
                original_count,
                keep_last,
                kept_count,
            } => {
                // Rebuild ctx.messages to match the agent's new
                // history: prepend a synthetic system entry holding
                // the summary, then keep the trailing `keep_last`
                // user turns + their assistant responses + any
                // tool blocks. Walk backwards counting user roles
                // to find the slice boundary so tool messages
                // attached to kept user turns ride along.
                let mut user_seen = 0usize;
                let mut slice_from = self.context.messages.len();
                for (idx, msg) in self.context.messages.iter().enumerate().rev() {
                    if msg.role == "user" {
                        user_seen += 1;
                        if user_seen > keep_last {
                            slice_from = idx + 1;
                            break;
                        }
                        slice_from = idx;
                    }
                }
                let kept_tail: Vec<crate::sessions::Message> =
                    self.context.messages.drain(slice_from..).collect();

                // Synthesize a system message for the summary so
                // it renders as a non-conversational block in the
                // chat pane.
                let summary_msg = crate::sessions::Message {
                    id: 0,
                    session_id: self.context.session_id.clone().unwrap_or_default(),
                    role: "system".to_string(),
                    content: format!(
                        "[Compacted summary of earlier conversation]\n\n{}",
                        summary.trim()
                    ),
                    tool_calls: None,
                    timestamp: chrono::Utc::now().timestamp(),
                };

                self.context.messages.clear();
                self.context.messages.push(summary_msg);
                self.context.messages.extend(kept_tail);

                // Persist the new shape so it survives restart (only when a
                // session is bound — compaction always runs on an active one).
                if let Some(sid) = self.context.session_id.clone() {
                    if let Err(e) = self
                        .context
                        .session_store
                        .replace_messages(&sid, &self.context.messages)
                    {
                        tracing::warn!("failed to persist compacted history: {e}");
                    }
                }

                // Returned to ready; the streaming working indicator
                // stops as soon as state flips.
                self.state = AppState::Ready;

                let _ = self.context.append_system_message(&format!(
                    "Compacted {original_count} → {kept_count} message(s). \
                     Older turns folded into the summary above."
                ));
            }
            // The TUI gates tools through its own inline `PendingApprovals`
            // overlay, never the web-modal backend, so it never receives this
            // event. Covered for exhaustiveness only.
            AgentEvent::ApprovalRequest { .. } => {}
        }
    }

    /// Finalize a turn on `AgentEvent::Done`.
    ///
    /// On cancel, the inline `Agent::turn_streaming` loop emits an empty
    /// `final_text` (it cannot easily salvage buffered partial text), so the
    /// TUI must reconstruct the visible output from the local `partial`
    /// accumulator built up from `Chunk` events. A `[cancelled]` marker is
    /// appended in that case so the user sees the interruption clearly.
    ///
    /// If more turns are queued, transitions back to a fresh `Streaming`
    /// state; otherwise returns to `Ready`.
    fn finalize_turn(&mut self, final_text: String, cancelled: bool) {
        // Reset per-turn UX counters before any state transitions so
        // the next turn starts with a clean slate.
        self.shell_blocks_this_turn = 0;
        self.autonomy_hint_shown_this_turn = false;
        let mut body = if cancelled && final_text.is_empty() {
            if let AppState::Streaming { partial, .. } = &self.state {
                partial.clone()
            } else {
                String::new()
            }
        } else {
            final_text
        };

        if cancelled {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str("[cancelled]");
        }

        // Provider returned no text after the tool loop — most often a
        // model that emits tool calls then stops without a final natural-
        // language summary (seen with MiniMax + multi-tool flows like
        // "help me setup gog skill"). Without this fallback the TUI
        // committed an empty Assistant: line, leaving the user staring
        // at a blank reply with no signal that the turn actually ended.
        // Salvage anything we streamed during the turn; otherwise show
        // an explicit "[no response]" so the user knows to retry.
        if !cancelled && body.is_empty() {
            if let AppState::Streaming { partial, .. } = &self.state {
                if !partial.is_empty() {
                    body = partial.clone();
                }
            }
            if body.is_empty() {
                body = "[no response from model — try /retry or rephrase]".to_string();
            }
        }

        // Snapshot tool blocks from streaming state before we transition
        // away — they're discarded otherwise.
        let tool_calls_json = if let AppState::Streaming { tool_blocks, .. } = &self.state {
            super::render::serialize_tool_calls(tool_blocks)
        } else {
            None
        };
        // Preserve a cloned list of the turn's tool calls so `/calls`
        // can render them after the turn ends. Replaces (not appends
        // to) the previous turn's list — `/calls` is "what did the
        // last turn do?", not a cross-turn history.
        if let AppState::Streaming { tool_blocks, .. } = &self.state {
            self.context.last_turn_tool_calls = tool_blocks
                .iter()
                .map(super::render::PersistedToolCall::from)
                .collect();
        }

        // Persist and display the assistant reply. A store failure should not
        // crash the loop — surface it as a visible error and keep running.
        let persist_ok = match self
            .context
            .append_assistant_message_with_tools(&body, tool_calls_json)
        {
            Ok(()) => true,
            Err(e) => {
                self.context.last_error = Some(format!("failed to persist reply: {e}"));
                false
            }
        };
        // Commit assistant message to scrollback (inline mode).
        // If we've already been streaming this turn line-by-line into
        // scrollback, only emit the trailing partial-line tail (if any)
        // plus a blank separator. Otherwise emit the full message.
        if self.stream_header_committed {
            let committed = self.stream_committed_chars.min(body.len());
            let tail = body[committed..].to_string();
            if tail.is_empty() {
                self.scrollback_queue
                    .push(("_continuation".to_string(), String::new()));
            } else {
                self.scrollback_queue
                    .push(("_continuation".to_string(), tail));
            }
        } else {
            self.scrollback_queue
                .push(("assistant".to_string(), body.clone()));
        }

        if self.context.queued_turns > 0 {
            self.context.queued_turns -= 1;
            self.state = AppState::Streaming {
                partial: String::new(),
                tool_blocks: Vec::new(),
                cancelling: false,
                turn_started_at: std::time::Instant::now(),
            };
            self.stream_committed_chars = 0;
            self.stream_header_committed = false;
        } else {
            self.state = AppState::Ready;
        }

        // A clean turn means the underlying system path that produced
        // the previous `last_error` is verifiably healthy now (the
        // provider responded, the model accepted the request, persist
        // succeeded). Cancelled turns and the persist-failure branch
        // above are NOT clean — they leave `last_error` alone (or, for
        // persist failure, leave the freshly-set error in place) so
        // the user keeps seeing the original cause until a turn
        // actually completes end-to-end.
        if !cancelled && persist_ok {
            self.context.last_error = None;
        }
    }

    /// Finalize a turn on `AgentEvent::Error`. Surfaces the error as a
    /// visible assistant message (so it shows up in chat history) AND sets
    /// `last_error` so the status bar reflects it until cleared.
    ///
    /// Recognizes a small list of common error shapes (API key not set,
    /// rate-limited, model not available) and rewrites them into a
    /// short, actionable line so the chat doesn't get a wall of stack
    /// trace. Unknown errors fall through verbatim.
    fn finalize_error(&mut self, msg: String) {
        // Reset per-turn UX counters here too, since errors are
        // alternative turn-end states.
        self.shell_blocks_this_turn = 0;
        self.autonomy_hint_shown_this_turn = false;
        let provider_hint = self
            .config
            .default_provider
            .clone()
            .unwrap_or_else(|| "openrouter".to_string());
        let (chat_body, status_line) = format_agent_error(&msg, &provider_hint);
        if let Err(e) = self.context.append_assistant_message(&chat_body) {
            self.context.last_error = Some(format!("failed to persist error: {e}"));
        } else {
            self.context.last_error = Some(status_line);
        }
        // Commit error message to scrollback so the user sees it inline.
        self.scrollback_queue
            .push(("system".to_string(), chat_body));
        self.state = AppState::Ready;
    }

    /// Handle a slash command (text after the leading `/`).
    pub async fn handle_command(&mut self, cmd: &str) -> Result<()> {
        match self.command_registry.dispatch(cmd, &mut self.context)? {
            CmdResult::Quit => {
                self.state = AppState::Quitting;
            }
            CmdResult::Message(msg) => {
                // Append as a system chat message so multi-line content
                // renders properly. The status bar's `last_error` slot is
                // reserved for errors only.
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
            }
            CmdResult::Overlay(content) => {
                // Inline-mode: render the overlay's body straight into
                // terminal scrollback (Hermes / Claude Code feel) so the
                // user can scroll back through long output natively. We
                // flatten the active tab plus a header line.
                let mut out = String::new();
                out.push_str(&format!("{}\n", content.title));
                for (i, tab) in content.tabs.iter().enumerate() {
                    if content.tabs.len() > 1 {
                        let marker = if i == content.active_tab {
                            "▸ "
                        } else {
                            "  "
                        };
                        out.push_str(&format!("\n{}{}\n", marker, tab.label));
                    }
                    for line in &tab.body {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                let _ = self.context.append_system_message(&out);
                self.scrollback_queue.push(("system".to_string(), out));
            }
            CmdResult::Continue | CmdResult::ClearError => {
                self.context.last_error = None;
            }
            CmdResult::Resubmit(text) => {
                self.dispatch_resubmit(text).await;
            }
            CmdResult::OpenListPicker(picker) => {
                self.list_picker = Some(picker);
            }
            CmdResult::OpenInfoPanel(panel) => {
                self.info_panel = Some(panel);
            }
            CmdResult::OpenSetupOverlay { provisioner } => {
                if let Some(name) = provisioner {
                    if let Err(e) = self.open_setup_overlay(name) {
                        let msg = format!("Failed to open setup: {}", e);
                        let _ = self.context.append_system_message(&msg);
                        self.scrollback_queue.push(("system".to_string(), msg));
                    }
                }
            }
            CmdResult::OpenSetupCategory { category } => {
                self.open_category_sub_picker(&category);
            }
            CmdResult::CancelTurn => {
                // Same path Esc takes. Only meaningful mid-turn; say so
                // rather than silently doing nothing when idle.
                if matches!(self.state, AppState::Streaming { .. }) {
                    if let AppState::Streaming { cancelling, .. } = &mut self.state {
                        *cancelling = true;
                    }
                    if let Err(e) = self.context.req_tx.send(TurnRequest::Cancel).await {
                        self.context.last_error = Some(format!("cancel failed: {e}"));
                    }
                } else {
                    let msg =
                        "Nothing is running — /stop cancels an in-flight turn (Esc does the same)."
                            .to_string();
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".to_string(), msg));
                }
            }
            CmdResult::OpenFirstRunWizard => {
                self.first_run_wizard = Some(super::FirstRunWizard::new(self.profile.clone()));
            }
            CmdResult::OpenClawhubInstallPicker { initial_query } => {
                self.open_clawhub_install_picker(initial_query).await;
            }
            CmdResult::ClearTerminal(announce) => {
                // The actual screen+scrollback wipe runs in `run_loop`
                // (which owns the Terminal). We just raise the flag and
                // queue the announcement to be emitted on the fresh
                // screen. Drop any pending scrollback so we don't echo
                // the user's `/new` line into the cleared terminal.
                self.scrollback_queue.clear();
                self.clear_terminal_request = true;
                let _ = self.context.append_system_message(&announce);
                self.scrollback_queue.push(("system".to_string(), announce));
            }
            CmdResult::SetInput(text) => {
                // Replace input buffer; cursor lands at end. Used by
                // `/<skill-name>` direct-invoke shortcut.
                self.context.input_buffer = text;
                self.context.cursor_to_end();
            }
        }
        Ok(())
    }

    /// Apply the user's selection from the active list picker. Matches
    /// on `ListPickerKind` so each picker type runs its own side effect
    /// (switch model, resume session, set personality…). Always closes
    /// the picker afterward.
    //
    // `async` is retained for parity with the sibling dispatch_resubmit
    // helper that does await on `req_tx.send(...)`; this one happens to
    // dispatch only sync arms today but is called via `.await` at the
    // single call site and may grow await points later. Suppress the
    // delta clippy gate so cosmetic edits inside this fn (e.g. the
    // `last_error = None` clear added with the error-lifecycle fix)
    // don't tip a long-standing baseline warning into a blocking error.
    #[allow(clippy::unused_async)]
    async fn dispatch_list_picker_selection(&mut self) {
        use super::widgets::ListPickerKind;

        let (kind, key) = match self
            .list_picker
            .as_ref()
            .and_then(|p| p.current().map(|item| (p.kind, item.key.clone())))
        {
            Some(v) => v,
            None => {
                self.list_picker = None;
                self.close_clawhub_install_picker_state();
                return;
            }
        };
        self.list_picker = None;
        self.close_clawhub_install_picker_state();
        self.close_skill_deps_install_state();

        match kind {
            ListPickerKind::Model => {
                self.context.model = key.clone();
                // See `commands/model.rs::execute` for the rationale —
                // an explicit model switch supersedes any model-related
                // last_error from the previous selection.
                self.context.last_error = None;
                let msg = format!("Model set to: {key}");
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
            }
            ListPickerKind::Session => {
                let session = match self.context.session_store.get_session(&key) {
                    Ok(Some(s)) => s,
                    Ok(None) => {
                        let msg = format!("Session {key} not found.");
                        let _ = self.context.append_system_message(&msg);
                        self.scrollback_queue.push(("system".to_string(), msg));
                        return;
                    }
                    Err(e) => {
                        self.context.last_error = Some(format!("resume failed: {e}"));
                        return;
                    }
                };
                self.context.session_id = Some(session.id.clone());
                self.context.model = session.model.clone();
                self.context.messages.clear();
                if let Err(e) = self.context.load_session_messages() {
                    self.context.last_error = Some(format!("load_session failed: {e}"));
                    return;
                }
                let short = &session.id[..session.id.len().min(8)];
                let msg = format!(
                    "Resumed session {short} ({} messages)",
                    self.context.messages.len()
                );
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
                // Replay the loaded messages into the scrollback so the
                // user can actually see the history. Without this, the
                // resume just shows "Resumed session ... (N messages)"
                // and an empty chat — the v0.6.1-alpha bug.
                for m in &self.context.messages {
                    self.scrollback_queue
                        .push((m.role.clone(), m.content.clone()));
                }
            }
            ListPickerKind::Personality => {
                let msg = format!(
                    "Personality set to: {key}\n(Full integration with system prompt pending)"
                );
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
            }
            ListPickerKind::Skill => {
                // Pre-fill an invocation prompt into the input buffer.
                // The user can edit, append context, and Enter to send.
                self.context.input_buffer = format!("Use the {key} skill: ");
                self.context.cursor_to_end();
                self.refresh_autocomplete();
            }
            ListPickerKind::Help => {
                // Pre-fill `/<command> ` into the input buffer so the
                // user can add args and submit (or just press Enter for
                // no-arg commands like /usage or /status).
                self.context.input_buffer = format!("/{key} ");
                self.context.cursor_to_end();
                self.refresh_autocomplete();
            }
            ListPickerKind::SetupTopic => match dispatch_setup_topic_key(&key) {
                SetupTopicAction::TuiProvisioner(ref name) => {
                    let name = name.clone();
                    match self.open_setup_overlay(name.clone()) {
                        Ok(()) => {}
                        Err(e) => {
                            let msg = format!("Failed to open {name} setup: {e}");
                            let _ = self.context.append_system_message(&msg);
                            self.scrollback_queue.push(("system".into(), msg));
                        }
                    }
                }
                SetupTopicAction::OpenCategorySubPicker(ref cat_key) => {
                    self.open_category_sub_picker(cat_key);
                }
                SetupTopicAction::Unknown => {
                    let msg = format!("Unknown setup topic: {key}");
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".into(), msg));
                }
            },
            ListPickerKind::SetupChannel => match dispatch_setup_channel_key(&key) {
                SetupChannelAction::TuiProvisioner(ref name) => {
                    let name = name.clone();
                    match self.open_setup_overlay(name.clone()) {
                        Ok(()) => {}
                        Err(e) => {
                            let msg = format!("Failed to open {name} setup: {e}");
                            let _ = self.context.append_system_message(&msg);
                            self.scrollback_queue.push(("system".into(), msg));
                        }
                    }
                }
                SetupChannelAction::Unknown => {
                    let msg = format!(
                        "Channel {key} is not yet available in-TUI. Run `rantaiclaw setup channels --non-interactive` from a shell to use the legacy CLI flow."
                    );
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".into(), msg));
                }
            },
            ListPickerKind::ClawhubInstall => {
                // ClawhubInstall Enter is handled inline in handle_key
                // (split between Focus::Search → search and Focus::List
                // → install) so it never reaches dispatch. This arm is
                // unreachable; left as a defensive no-op.
            }
            ListPickerKind::Autonomy => {
                use crate::approval::policy_writer::{self, PolicyPreset};
                let target = match PolicyPreset::from_str_ci(&key) {
                    Ok(p) => p,
                    Err(e) => {
                        let msg = format!("Unknown autonomy preset key '{key}': {e}");
                        let _ = self.context.append_system_message(&msg);
                        self.scrollback_queue.push(("system".into(), msg));
                        return;
                    }
                };
                let warning = match policy_writer::write_policy_files(&self.profile, target, true) {
                    Ok(w) => w,
                    Err(e) => {
                        let msg = format!("Failed to switch autonomy mode: {e}");
                        let _ = self.context.append_system_message(&msg);
                        self.scrollback_queue.push(("system".into(), msg));
                        return;
                    }
                };
                if let Err(e) = self.apply_preset_to_config_and_reload(target) {
                    let msg = format!("⚠ Preset written, but live reload failed: {e}");
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".into(), msg));
                }
                self.context.autonomy_preset = Some(target);
                let msg = format!(
                    "⚙ Autonomy mode → {} ({}). Shift+Tab to cycle · /autonomy to pick.",
                    target.label(),
                    preset_blurb(target),
                );
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".into(), msg));
                if let Some(w) = warning {
                    let _ = self.context.append_system_message(w);
                    self.scrollback_queue.push(("system".into(), w.to_string()));
                }
            }
        }
    }

    /// Open the ClawHub install picker. Empty query → top-by-stars listing.
    /// Search fires only on Enter while focused on the search bar (per
    /// tester request — keystroke-fire churned the network too aggressively).
    async fn open_clawhub_install_picker(&mut self, initial_query: Option<String>) {
        self.open_clawhub_install_picker_sync(initial_query);
    }

    /// Sync variant — same as `open_clawhub_install_picker` but callable
    /// from inside synchronous contexts like `drain_events`. Spawns the
    /// initial fetch via `tokio::spawn` so we never block the render loop.
    fn open_clawhub_install_picker_sync(&mut self, initial_query: Option<String>) {
        use super::widgets::{ListPicker, ListPickerKind};

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.clawhub_install_results_rx = Some(rx);
        self.clawhub_install_results_tx = Some(tx);
        self.clawhub_install_last_query = String::new();
        // Note: don't reset `search_version` — keep monotonic so any
        // straggling task from a prior picker session can't resurrect
        // its results into this one.

        let mut picker = ListPicker::new(
            ListPickerKind::ClawhubInstall,
            "Install Skill",
            Vec::new(),
            None,
            "Loading top ClawHub skills…",
        );
        if let Some(q) = initial_query.as_deref() {
            picker.query = q.to_string();
        }
        let starting_query = picker.query.clone();
        self.list_picker = Some(picker);
        self.spawn_clawhub_search(&starting_query);
    }

    /// Spawn an async ClawHub fetch for the given query. Empty string
    /// means "give me the top-by-stars listing". Each spawn bumps the
    /// search version so older inflight tasks' results are dropped on
    /// arrival (avoids races when the user types quickly).
    fn spawn_clawhub_search(&mut self, query: &str) {
        let Some(tx) = self.clawhub_install_results_tx.clone() else {
            return;
        };
        // Clear the "↵ Enter to search ClawHub" hint — a search is
        // about to fire, so the typed query is no longer pending.
        if let Some(p) = self.list_picker.as_mut() {
            p.search_pending = false;
        }
        self.clawhub_install_search_version = self.clawhub_install_search_version.wrapping_add(1);
        let version = self.clawhub_install_search_version;
        let q = query.to_string();
        tokio::spawn(async move {
            let result = if q.trim().is_empty() {
                crate::skills::clawhub::list_top(50).await
            } else {
                crate::skills::clawhub::search(&q).await
            };
            let _ = tx.send((version, result));
        });
    }

    fn spawn_skill_deps_install(&mut self) {
        if self.skill_deps_install_in_progress.is_some() {
            return;
        }
        let Some(skill_name) = self
            .list_picker
            .as_ref()
            .and_then(|p| p.current().map(|item| item.key.clone()))
        else {
            return;
        };
        let Some(skill) = self
            .context
            .available_skills_with_status
            .iter()
            .map(|(skill, _)| skill)
            .chain(self.context.available_skills.iter())
            .find(|s| s.name.eq_ignore_ascii_case(&skill_name))
            .cloned()
        else {
            if let Some(p) = self.list_picker.as_mut() {
                p.title = format!("Skills · {skill_name} not found");
            }
            return;
        };

        if skill.install_recipes.is_empty() {
            if let Some(p) = self.list_picker.as_mut() {
                p.title = format!("Skills · no install-deps recipe for {}", skill.name);
            }
            return;
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<
            Result<crate::skills::install_deps::InstallDepsOutcome, String>,
        >();
        self.skill_deps_install_completion_rx = Some(rx);
        self.skill_deps_install_completion_tx = Some(tx.clone());
        self.skill_deps_install_in_progress = Some((skill.name.clone(), 0));

        let prefs =
            crate::skills::install_deps::SelectorPrefs::from_config(&self.config.skills.install);
        tokio::task::spawn_blocking(move || {
            let result = crate::skills::install_deps::install_deps_for_with_prefs(&skill, &prefs)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
    }

    /// Drain debounced reload ticks from the `config.toml` file
    /// watcher. Direct user edits to the active profile's config
    /// (adding `[mcp_servers.foo]`, swapping provider, etc.) trigger
    /// the same `reload_config` pipeline the wizard close uses.
    ///
    /// As of v0.6.53-alpha the *success* path is silent in the TUI:
    /// every wizard-driven flow nudges config.toml multiple times in
    /// succession, and surfacing each one as a System line spammed the
    /// chat history (`config.toml changed — agent reloaded` repeated
    /// 3-5× per /setup run). The reload still happens; the log line
    /// goes to `tracing::info!` so it's recoverable in debug logs.
    /// Failures still surface in the TUI — those are real user-actionable
    /// signals.
    fn drain_config_reload_events(&mut self) {
        let mut should_reload = false;
        if let Some(watcher) = self.config_watcher.as_mut() {
            while watcher.reload_rx.try_recv().is_ok() {
                should_reload = true;
            }
        }
        if !should_reload {
            return;
        }
        match self.reload_config() {
            Ok(()) => {
                tracing::info!(
                    "config.toml changed — agent reloaded; next turn uses the new settings"
                );
            }
            Err(e) => {
                let msg = format!("⚠ config.toml changed but reload failed: {e}");
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
            }
        }
    }

    fn drain_skill_reload_events(&mut self) {
        let mut should_reload = false;
        if let Some(watcher) = self.skills_watcher.as_mut() {
            while watcher.reload_rx.try_recv().is_ok() {
                should_reload = true;
            }
        }
        if !should_reload {
            return;
        }

        // Compare skills before/after refresh so we only push a full
        // `TurnRequest::Reload` to the agent when the *set of skills*
        // actually changed. notify can fire on innocuous fs noise
        // (editor temp files, mtime touches) — rebuilding the agent on
        // every tick would respawn MCP servers and rerun discovery for
        // nothing.
        let prev_keys: std::collections::HashSet<String> = self
            .context
            .available_skills
            .iter()
            .map(|s| s.name.clone())
            .collect();
        self.refresh_available_skills();
        let new_keys: std::collections::HashSet<String> = self
            .context
            .available_skills
            .iter()
            .map(|s| s.name.clone())
            .collect();
        if prev_keys != new_keys {
            // Push a full reload to the agent actor so the next turn's
            // system prompt reflects the new skill list. Uses the
            // existing `TurnRequest::Reload` path (same as wizard
            // close) — costs ~one Agent rebuild including MCP
            // re-discovery, but only on real skill add/remove.
            let config = self.config.clone();
            let req_tx = self.context.req_tx.clone();
            tokio::spawn(async move {
                let _ = req_tx
                    .send(crate::tui::TurnRequest::Reload(Box::new(config)))
                    .await;
            });
            tracing::info!(
                target: "tui",
                added = ?new_keys.difference(&prev_keys).collect::<Vec<_>>(),
                removed = ?prev_keys.difference(&new_keys).collect::<Vec<_>>(),
                "skills changed on disk — dispatching agent reload"
            );
            // The channel runtime bakes the skill list into its system
            // prompt at build time (see `start_channels`), so the local
            // agent reload above does not reach running channel listeners
            // (Telegram/Discord/…). Restart the runtime in place — only
            // when one is actually running — so the channel bot picks up the
            // new skill set too. Skipped when no channels run, to avoid
            // spurious teardown on skill edits in a pure local-chat session.
            if self.channel_supervisor.is_some() {
                let _ = self
                    .context
                    .append_system_message("✓ Skills changed — refreshing channel bot(s) now.");
                self.restart_channels();
            }
        }

        let items = self.skill_picker_items();
        let suppress_title = self.skill_deps_install_in_progress.is_some()
            || self
                .skill_deps_install_finished_at
                .is_some_and(|finished| finished.elapsed() < Duration::from_secs(3));
        if let Some(picker) = self.list_picker.as_mut() {
            if picker.kind == crate::tui::widgets::ListPickerKind::Skill {
                // Bail when nothing actually changed. The notify watcher
                // can fire on innocuous fs noise (editor saves in a
                // sibling tree, mtime touches, etc.) and rebuilding the
                // picker would reset cursor + page back to the top —
                // making it look like the picker auto-scrolls home every
                // second when the user is paging through their skills.
                let unchanged = picker.entries().len() == items.len()
                    && picker
                        .entries()
                        .iter()
                        .zip(items.iter())
                        .all(|(entry, new)| {
                            entry.as_item().is_some_and(|cur| {
                                cur.key == new.key
                                    && cur.primary == new.primary
                                    && cur.secondary == new.secondary
                            })
                        });
                if unchanged {
                    return;
                }
                // Items did change — preserve the cursor on the same
                // skill (by key) so the user doesn't lose their place
                // mid-scroll when the watcher fires.
                let preserved_key = picker.current().map(|i| i.key.clone());
                picker.set_items(items);
                if let Some(key) = preserved_key {
                    if let Some(abs_idx) = picker
                        .entries()
                        .iter()
                        .position(|e| e.as_item().is_some_and(|i| i.key == key))
                    {
                        // The picker's live page size, not a constant: it is
                        // derived from the rendered list area, so a hardcoded
                        // value would land the cursor on a different skill.
                        let page_size = picker.page_size();
                        picker.page = abs_idx / page_size;
                        picker.selected = abs_idx % page_size;
                        picker.list_state.select(Some(picker.selected));
                    }
                }
                if !suppress_title {
                    picker.title = "Skills · reloaded".to_string();
                }
            }
        }
    }

    /// Drain any pending ClawHub search results and apply the latest one
    /// matching the current search version. Called on each event loop
    /// tick (after a key is processed) so results land before the next
    /// render. Stale results (older versions) are silently discarded.
    fn drain_clawhub_search_results(&mut self) {
        use super::widgets::{ListPickerItem, ListPickerKind};

        let current_version = self.clawhub_install_search_version;
        let Some(rx) = self.clawhub_install_results_rx.as_mut() else {
            return;
        };

        let mut latest: Option<anyhow::Result<Vec<crate::skills::clawhub::ClawHubSkill>>> = None;
        while let Ok((version, result)) = rx.try_recv() {
            if version == current_version {
                latest = Some(result);
            }
        }

        let Some(result) = latest else {
            return;
        };

        let Some(picker) = self.list_picker.as_mut() else {
            return;
        };
        if picker.kind != ListPickerKind::ClawhubInstall {
            return;
        }

        match result {
            Ok(skills) => {
                let items: Vec<ListPickerItem> = skills
                    .into_iter()
                    .map(|s| {
                        let name = if s.display_name.is_empty() {
                            s.slug.clone()
                        } else {
                            s.display_name.clone()
                        };
                        // Listings include star counts; search results
                        // don't (server returns score, not stats), so
                        // omit the (★N) suffix when stars is zero to
                        // avoid showing a misleading "0 stars" for every
                        // search hit.
                        let primary = if s.stats.stars > 0 {
                            format!("{name}  (★{})", s.stats.stars)
                        } else {
                            name
                        };
                        let secondary = if s.summary.is_empty() {
                            String::new()
                        } else {
                            let cleaned = s.summary.replace('\n', " ");
                            let cleaned = cleaned.trim();
                            if cleaned.chars().count() > 90 {
                                let head: String = cleaned.chars().take(87).collect();
                                format!("{head}…")
                            } else {
                                cleaned.to_string()
                            }
                        };
                        ListPickerItem {
                            key: s.slug,
                            primary,
                            secondary,
                        }
                    })
                    .collect();
                picker.set_items(items);
            }
            Err(e) => {
                tracing::warn!("ClawHub search failed: {e}");
            }
        }
    }

    /// Per-frame poll for ClawHub install progress: advances the spinner
    /// animation (visible as the picker title) and reacts to completion
    /// when the spawned install task posts its result. Called from
    /// `drain_events`, which itself runs at the start of each render
    /// frame, so the spinner advances every poll tick (~100 ms idle,
    /// ~16 ms while streaming).
    fn tick_clawhub_install(&mut self) {
        // Braille-dot spinner. Same set used by `cargo`, so it's visually
        // familiar and renders cleanly in any monospace font.
        const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

        let Some((slug, frame)) = self.clawhub_install_in_progress.as_mut() else {
            return;
        };

        // Drain completion channel — only the most recent result matters
        // (only one install can be in flight at a time, but be defensive).
        let mut completion: Option<Result<String, String>> = None;
        if let Some(rx) = self.clawhub_install_completion_rx.as_mut() {
            while let Ok(msg) = rx.try_recv() {
                completion = Some(msg);
            }
        }

        match completion {
            None => {
                // Still installing — animate the spinner.
                *frame = (*frame + 1) % SPINNER.len();
                let glyph = SPINNER[*frame];
                if let Some(p) = self.list_picker.as_mut() {
                    p.title = format!("{glyph}  Installing {slug}…");
                }
            }
            Some(Ok(installed_slug)) => {
                // Reload skills so the new one is visible to /skills and
                // to the next agent turn.
                self.refresh_available_skills();
                if self.wizard_install_in_progress {
                    self.wizard_installed_slugs.push(installed_slug.clone());
                }

                // Swap the ClawhubInstall picker for the standard Skill
                // picker, preselecting the freshly-installed slug — this
                // is the "throw us to /skills" UX the tester asked for.
                self.close_clawhub_install_picker_state();
                self.clawhub_install_in_progress = None;
                self.clawhub_install_completion_rx = None;
                self.clawhub_install_completion_tx = None;
                let items = self.skill_picker_items();
                self.list_picker = Some(crate::tui::widgets::ListPicker::new(
                    crate::tui::widgets::ListPickerKind::Skill,
                    format!("Skills · ✓ Installed {installed_slug}"),
                    items,
                    Some(&installed_slug),
                    "No skills loaded.",
                ));
            }
            Some(Err(error_msg)) => {
                // Surface failure in the picker title; keep the overlay
                // open so the user can pick a different slug or retry.
                if let Some(p) = self.list_picker.as_mut() {
                    p.title = format!("✗ Install failed: {error_msg}");
                }
                self.clawhub_install_in_progress = None;
                self.clawhub_install_completion_rx = None;
                self.clawhub_install_completion_tx = None;
            }
        }
    }

    fn tick_skill_deps_install(&mut self) {
        const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

        let Some((skill, frame)) = self.skill_deps_install_in_progress.as_mut() else {
            return;
        };

        let mut completion = None;
        if let Some(rx) = self.skill_deps_install_completion_rx.as_mut() {
            while let Ok(msg) = rx.try_recv() {
                completion = Some(msg);
            }
        }

        match completion {
            None => {
                *frame = (*frame + 1) % SPINNER.len();
                let glyph = SPINNER[*frame];
                if let Some(p) = self.list_picker.as_mut() {
                    p.title = format!("{glyph}  Installing deps for {skill}…");
                }
            }
            Some(Ok(outcome)) => {
                self.skill_deps_install_finished_at = Some(Instant::now());
                self.refresh_available_skills();
                let items = self.skill_picker_items();
                if let Some(p) = self.list_picker.as_mut() {
                    if p.kind == crate::tui::widgets::ListPickerKind::Skill {
                        p.set_items(items);
                    }
                    if outcome.bins_still_missing.is_empty() {
                        if outcome.bins_installed.is_empty() {
                            p.title =
                                format!("Skills · deps already satisfied for {}", outcome.skill);
                        } else {
                            p.title = format!(
                                "Skills · ✓ installed {}",
                                outcome.bins_installed.join(", ")
                            );
                        }
                    } else {
                        p.title = format!(
                            "Skills · still missing {}",
                            outcome.bins_still_missing.join(", ")
                        );
                    }
                }
                self.close_skill_deps_install_state();
            }
            Some(Err(error_msg)) => {
                self.skill_deps_install_finished_at = Some(Instant::now());
                if let Some(p) = self.list_picker.as_mut() {
                    p.title = format!("Skills · install-deps failed: {error_msg}");
                }
                self.close_skill_deps_install_state();
            }
        }
    }

    /// Tear down ClawHub install picker async state. Called when the
    /// picker closes (Enter/Esc) so any inflight task's result can't
    /// resurrect a closed picker.
    fn close_clawhub_install_picker_state(&mut self) {
        self.clawhub_install_results_rx = None;
        self.clawhub_install_results_tx = None;
        self.clawhub_install_last_query = String::new();
        // Don't reset version — keep it monotonic so any straggling
        // task's send to a since-dropped tx is a no-op AND if the user
        // reopens the picker mid-flight, fresh searches don't collide
        // with old version numbers.

        // If this picker was opened from inside the first-run wizard's
        // skills step, the wizard is awaiting an InstalledSkills response.
        // Send it now (with whatever was installed during the session)
        // so the wizard can advance to the next provisioner.
        if self.wizard_install_in_progress {
            self.wizard_install_in_progress = false;
            let installed = std::mem::take(&mut self.wizard_installed_slugs);
            if let Some(tx) = self.setup_response_tx.as_ref() {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = tx
                        .send(
                            crate::onboard::provision::ProvisionResponse::InstalledSkills(
                                installed,
                            ),
                        )
                        .await;
                });
            }
        }
    }

    fn close_skill_deps_install_state(&mut self) {
        self.skill_deps_install_in_progress = None;
        self.skill_deps_install_completion_rx = None;
        self.skill_deps_install_completion_tx = None;
    }

    fn open_setup_overlay(&mut self, name: String) -> anyhow::Result<()> {
        use crate::onboard::provision::provisioner_for;

        let prov =
            provisioner_for(&name).ok_or_else(|| anyhow::anyhow!("unknown provisioner: {name}"))?;

        let (events_tx, events_rx) = tokio::sync::mpsc::channel(32);
        let (response_tx, response_rx) = tokio::sync::mpsc::channel(8);
        // Oneshot the spawn task uses to send the post-save Config back
        // to the main loop. The race fix: see `setup_save_complete_rx`
        // field doc — we MUST swap `self.config` to the saved value
        // BEFORE the wizard opens the next provisioner overlay,
        // otherwise N+1 clones a stale self.config and saves over N.
        let (save_tx, save_rx) = tokio::sync::oneshot::channel::<crate::config::Config>();

        let mut config = self.config.clone();
        let profile = self.profile.clone();

        let prov_name = prov.name().to_string();
        let overlay_state = crate::tui::SetupOverlayState::new(format!("Setup — {prov_name}"));

        self.setup_overlay = Some(overlay_state);
        self.setup_event_rx = Some(events_rx);
        self.setup_response_tx = Some(response_tx);
        self.setup_save_complete_rx = Some(save_rx);

        let events_tx = events_tx;
        tokio::spawn(async move {
            // Clone events_tx so we can still report save failures to the
            // overlay after `prov.run` consumes the original via ProvisionIo.
            let save_failure_tx = events_tx.clone();
            let io = crate::onboard::provision::ProvisionIo {
                events: events_tx,
                responses: response_rx,
            };
            match prov.run(&mut config, &profile, io).await {
                Ok(()) => {
                    // Persist the mutated config to disk. Without this,
                    // every provisioner mutation is lost when the spawned
                    // task drops `config`. Config::save() also handles
                    // secret encryption (see config/schema.rs:save) so
                    // plaintext API keys captured during the flow get
                    // encrypted before hitting disk.
                    //
                    // Defensive: if `config.config_path` somehow ended
                    // up empty (e.g. a Default::default() Config slipped
                    // through, or a serde-skipped reload), fall back to
                    // the profile's known config.toml path so save()
                    // can compute a parent dir.
                    if config.config_path.parent().is_none() {
                        config.config_path = profile.config_toml();
                    }
                    match config.save().await {
                        Ok(()) => {
                            // Hand the just-saved Config back to the main
                            // loop. The render-tick drain swaps it into
                            // self.config and only then does the wizard
                            // advance to the next provisioner. Closes the
                            // race where the next overlay would clone a
                            // stale self.config and overwrite this save.
                            let _ = save_tx.send(config);
                        }
                        Err(e) => {
                            tracing::error!(
                                provisioner = prov_name,
                                "failed to save config after provisioner: {e}"
                            );
                            // Best-effort surface to the overlay log so
                            // the user sees the failure instead of a
                            // phantom success. save_tx is dropped here →
                            // receiver gets `Closed`, which the gate
                            // treats as "save failed, don't advance".
                            let _ = save_failure_tx
                                .send(crate::onboard::provision::ProvisionEvent::Failed {
                                    error: format!("Config save failed: {e}"),
                                })
                                .await;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(provisioner = prov_name, "provisioner error: {e}");
                    // Surface the error to the overlay so the user
                    // sees what went wrong and the wizard's failure
                    // detection (overlay.failure_reason) fires.
                    // Without this the overlay's `finished` flag is
                    // never set and the wizard freezes forever.
                    let _ = save_failure_tx
                        .send(crate::onboard::provision::ProvisionEvent::Failed {
                            error: format!("Provisioner error: {e}"),
                        })
                        .await;
                    // save_tx drops → receiver gets `Closed`.
                }
            }
        });

        Ok(())
    }

    /// Dispatch a `/retry`-style resubmit: re-runs an existing user message
    /// without appending it to history. Refuses while a turn is already
    /// streaming — the user should cancel first.
    async fn dispatch_resubmit(&mut self, text: String) {
        match self.state {
            AppState::Streaming { .. } => {
                self.context.last_error = Some(
                    "Cannot retry while a response is streaming. Press Ctrl+C to cancel first."
                        .to_string(),
                );
            }
            AppState::Ready => {
                if let Err(e) = self.context.req_tx.send(TurnRequest::Submit(text)).await {
                    self.context.last_error = Some(format!("agent bridge closed: {e}"));
                    return;
                }
                self.state = AppState::Streaming {
                    partial: String::new(),
                    tool_blocks: Vec::new(),
                    cancelling: false,
                    turn_started_at: std::time::Instant::now(),
                };
                self.context.last_error = None;
            }
            AppState::Quitting => {}
        }
    }

    /// Build and open a sub-picker showing only items in the given
    /// category. `cat_key` is one of `core` / `channel` /
    /// `integration` / `runtime` / `hardware` / `routing`.
    fn open_category_sub_picker(&mut self, cat_key: &str) {
        use crate::onboard::provision::{available, provisioner_for};
        use crate::tui::commands::setup::{cat_label, category_from_key};
        use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

        let Some(category) = category_from_key(cat_key) else {
            let msg = format!("Unknown setup category: {cat_key}");
            let _ = self.context.append_system_message(&msg);
            self.scrollback_queue.push(("system".into(), msg));
            return;
        };

        let items: Vec<ListPickerItem> = available()
            .into_iter()
            .filter_map(|(name, desc)| {
                let p = provisioner_for(name)?;
                if p.category() == category {
                    Some(ListPickerItem {
                        key: name.to_string(),
                        primary: name.to_string(),
                        secondary: desc.to_string(),
                    })
                } else {
                    None
                }
            })
            .collect();

        let title = format!("{} setup", cat_label(category));
        let empty_hint = format!(
            "no {} provisioners available",
            cat_label(category).to_lowercase()
        );

        let picker = ListPicker::new(
            ListPickerKind::SetupChannel, // re-used as the generic "category sub-picker" kind
            title,
            items,
            None,
            empty_hint,
        );
        self.list_picker = Some(picker);
    }

    /// Render the inline viewport — only the bottom `INLINE_VIEWPORT_LINES`
    /// rows of the terminal. Chat history is NOT rendered here; messages
    /// commit to scrollback via `commit_message_to_scrollback` as they
    /// arrive. The viewport just hosts: status bar, input box, and any
    /// transient overlays (autocomplete dropdown, /help modal, streaming
    /// spinner).
    pub fn render(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let TuiApp {
            state,
            context,
            autocomplete,
            overlay,
            list_picker,
            info_panel,
            stream_committed_chars,
            ..
        } = self;

        let pending_approval_snapshot = self.pending_approval.clone();
        terminal.draw(|frame| {
            let area = frame.area();

            // Tight 5-row layout: input + status. The pre-v0.6.50 layout
            // also reserved a stream-preview row above the input box that
            // duplicated the cancelling/thinking/streaming indicator
            // already shown in the status bar. Two indicators ticking at
            // once were noisy + confusing per tester feedback, so the
            // upper pane was removed and the status bar is now the
            // canonical streaming surface.
            let _ = stream_committed_chars; // kept on App for future re-introduction
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(4), Constraint::Length(1)])
                .split(area);

            // When an approval is pending, the top row swaps from the
            // input box to the styled approval prompt — same height so
            // the status bar stays put. The key handler absorbs Y/A/N/
            // Esc while the prompt is up; everything else falls through
            // (so Ctrl+C still cancels, /quit still works, etc.).
            if let Some(ref req) = pending_approval_snapshot {
                render_approval_pane(req, frame, chunks[0]);
            } else {
                render_input_pane(context, frame, chunks[0]);
            }
            render_status_pane(context, state, frame, chunks[1]);

            // Modal overlay (e.g. /help) takes over the entire viewport.
            if let Some(content) = overlay.as_ref() {
                render_overlay_pane(content, frame, area);
            }

            // Setup overlay — full terminal coverage while active. Drawn
            // BEFORE the list picker so that when the wizard's skills
            // step opens the install picker, the picker covers the
            // overlay (ClawhubInstall hand-off path) — otherwise the
            // user just sees the overlay's "Fetching…" log forever.
            if let Some(overlay_state) = self.setup_overlay.as_mut() {
                overlay_state.render(frame, area);
            }

            // List picker overlay — covers the entire 6-row viewport.
            if let Some(picker) = list_picker.as_mut() {
                picker.render(frame, area);
            }

            // Info panel overlay — read-only modal for /channels, /config,
            // /doctor, /insights, /status, /usage, /skill (no args).
            // Visually consistent with the list picker; same key dialect.
            if let Some(panel) = info_panel.as_ref() {
                panel.render(frame, area);
            }

            // First-run wizard — full terminal coverage, renders over everything.
            if let Some(wizard) = &mut self.first_run_wizard {
                wizard.render_fullscreen(frame, area);
            }

            // Note: the console login gate is a full-screen surface handled
            // by the alt-screen render path (see `want_alt`), never inline.

            // Slash-command dropdown — anchored just above the input box,
            // clamped to stay strictly inside the inline viewport.
            if autocomplete.is_visible() {
                let input_area = chunks[1];
                let space_above = input_area.y.saturating_sub(area.y);
                let max_rows: u16 = 8;
                let desired = (max_rows + 2).min(area.height.saturating_sub(1));
                let popup_height = desired.min(space_above.max(input_area.height));
                if popup_height >= 3 {
                    let popup_y = if space_above >= popup_height {
                        input_area.y - popup_height
                    } else {
                        input_area.y
                    };
                    let popup_area = Rect {
                        x: input_area.x,
                        y: popup_y,
                        width: input_area.width,
                        height: popup_height,
                    };
                    autocomplete.render(frame, popup_area);
                }
            }
        })?;
        Ok(())
    }

    /// Render path while the list picker is open. Uses the full
    /// terminal area (alt-screen mode) so the picker can show a search
    /// bar, many list rows, and a hotkey footer — Hermes / Claude-Code
    /// style. The status bar and input box are intentionally hidden
    /// here; the user is in modal selection mode.
    pub fn render_fullscreen_picker(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let TuiApp { list_picker, .. } = self;
        terminal.draw(|frame| {
            let area = frame.area();
            if let Some(picker) = list_picker.as_mut() {
                picker.render_fullscreen(frame, area);
            }
        })?;
        Ok(())
    }

    /// Fullscreen render for the read-only info panel. Mirrors the
    /// list-picker fullscreen path so /channels, /config, /doctor, etc.
    /// occupy the entire viewport while open and don't compete with the
    /// chat scrollback for screen real estate.
    pub fn render_fullscreen_info_panel(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let TuiApp { info_panel, .. } = self;
        terminal.draw(|frame| {
            let area = frame.area();
            if let Some(panel) = info_panel.as_ref() {
                panel.render(frame, area);
            }
        })?;
        Ok(())
    }

    /// Render path while the slash-command autocomplete dropdown is
    /// visible (alt-screen mode). Layout: input box at top, dropdown
    /// below (taking the bulk of the screen so many commands are
    /// visible at once), status bar at bottom — matches the Claude-Code
    /// reference image. The user keeps typing into the input; the
    /// dropdown re-filters live as they go.
    pub fn render_fullscreen_autocomplete(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let TuiApp {
            state,
            context,
            autocomplete,
            ..
        } = self;
        terminal.draw(|frame| {
            let area = frame.area();
            // Layout: 1 row top margin · 4 rows input · 1 row spacer ·
            // remaining rows for dropdown · 1 row status. Input pinned
            // near the top so typing position stays consistent with
            // inline mode; dropdown gets all the leftover height.
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // top margin
                    Constraint::Length(4), // input box
                    Constraint::Length(1), // spacer
                    Constraint::Min(3),    // dropdown
                    Constraint::Length(1), // status bar
                ])
                .split(area);

            render_input_pane(context, frame, chunks[1]);
            autocomplete.render(frame, chunks[3]);
            render_status_pane(context, state, frame, chunks[4]);
        })?;
        Ok(())
    }

    /// Commit a finalized message to the terminal's scrollback (above the
    /// inline viewport). This is the inline-mode equivalent of "append to
    /// chat history" — once committed the line is permanent and scrolls
    /// naturally with the terminal, exactly like Hermes / Claude Code.
    pub fn commit_message_to_scrollback(
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        role: &str,
        content: &str,
    ) -> Result<()> {
        let size = terminal.size()?;
        let theme = super::render::RenderTheme::default();
        let mut lines = if role == "_continuation" {
            // Bare content lines (no role header) — used to flush the
            // trailing partial line after a streaming turn.
            content
                .split('\n')
                .map(|l| super::render::render_block_line(l, &theme))
                .collect::<Vec<_>>()
        } else if role == "_tool_log" {
            // Inline tool-call summary, indented and muted so it sits
            // visually between assistant/user lines without competing
            // for attention. Single-line — already trimmed upstream.
            let muted = Style::default().fg(ratatui::style::Color::Rgb(107, 114, 128));
            vec![Line::from(vec![
                Span::raw("  "),
                Span::styled(content.to_string(), muted),
            ])]
        } else {
            super::render::render_message_lines(role, content, &[], &[], &theme)
        };
        lines.push(Line::from(""));
        commit_lines_to_scrollback(terminal, lines, size.width, size.height)
    }

    /// Flush newly-completed lines from the active streaming `partial`
    /// into the terminal scrollback, splitting on `\n`. Each call commits
    /// only the bytes that have a trailing newline since the last flush;
    /// the still-incomplete tail stays in `partial` until either more
    /// data arrives or the turn finalizes. Idempotent on a finalized
    /// state.
    pub fn flush_stream_to_scrollback(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        let partial = match &self.state {
            AppState::Streaming { partial, .. } => partial.clone(),
            _ => return Ok(()),
        };
        if self.stream_committed_chars > partial.len() {
            self.stream_committed_chars = 0;
        }
        let remaining = &partial[self.stream_committed_chars..];
        let last_nl = match remaining.rfind('\n') {
            Some(i) => i,
            None => return Ok(()),
        };
        let chunk = remaining[..=last_nl].to_string();
        let abs_end = self.stream_committed_chars + last_nl + 1;

        let theme = super::render::RenderTheme::default();
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut iter = chunk.split_inclusive('\n');
        if !self.stream_header_committed {
            if let Some(first_line) = iter.next() {
                let trimmed = first_line.trim_end_matches('\n');
                let body = super::render::render_block_line(trimmed, &theme);
                let mut spans = vec![Span::styled(
                    "Assistant: ",
                    Style::default()
                        .fg(theme.assistant_label)
                        .add_modifier(Modifier::BOLD),
                )];
                spans.extend(body.spans);
                lines.push(Line::from(spans));
            }
            self.stream_header_committed = true;
        }
        for rest in iter {
            let trimmed = rest.trim_end_matches('\n');
            lines.push(super::render::render_block_line(trimmed, &theme));
        }

        if !lines.is_empty() {
            let size = terminal.size()?;
            commit_lines_to_scrollback(terminal, lines, size.width, size.height)?;
        }
        self.stream_committed_chars = abs_end;
        Ok(())
    }

    /// Print the splash banner + welcome line once, into scrollback,
    /// before the inline viewport takes over the bottom of the terminal.
    pub fn commit_splash_to_scrollback(
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
        ctx: &TuiContext,
    ) -> Result<()> {
        let size = terminal.size()?;
        // Splash already shows `Rantaiclaw v<x>` in the right pane;
        // append only the session identifier here so the same info
        // doesn't appear twice in scrollback.
        let lines = render_splash_lines(ctx, size.width);
        let session_short = ctx.session_id_short();
        let mut all_lines = lines;
        all_lines.push(Line::from(Span::styled(
            format!("  · session {} ", session_short),
            Style::default().fg(Color::Rgb(107, 114, 128)),
        )));
        all_lines.push(Line::from(""));
        commit_lines_to_scrollback(terminal, all_lines, size.width, size.height)
    }

    /// Original `render_header` shape, kept for backward callers.
    #[allow(dead_code)]
    fn render_header(&self, frame: &mut ratatui::Frame, area: Rect) {
        let session_short = self.context.session_id_short();
        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                "  Rantaiclaw  ",
                Style::default()
                    .fg(Color::Rgb(4, 11, 46))
                    .bg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" v{}  ", env!("CARGO_PKG_VERSION")),
                Style::default()
                    .fg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("· session {} ", session_short),
                Style::default().fg(Color::Rgb(107, 114, 128)),
            ),
        ]));
        frame.render_widget(header, area);
    }

    /// Render the scrollable chat history. Shows a brand splash when the
    /// session is empty (mirrors Hermes' opening screen).
    fn render_chat(&self, frame: &mut ratatui::Frame, area: Rect) {
        let theme = super::render::RenderTheme::default();
        let mut items: Vec<ListItem> = Vec::with_capacity(self.context.messages.len() + 1);

        // Empty-state splash — figlet wordmark + welcome line.
        if self.context.messages.is_empty() && !matches!(self.state, AppState::Streaming { .. }) {
            for line in render_splash_lines(&self.context, area.width) {
                items.push(ListItem::new(line));
            }
        }

        for msg in &self.context.messages {
            let persisted = msg
                .tool_calls
                .as_deref()
                .map(super::render::parse_persisted_tool_calls)
                .unwrap_or_default();
            let lines = super::render::render_message_lines(
                &msg.role,
                &msg.content,
                &persisted,
                &[],
                &theme,
            );
            items.push(ListItem::new(lines));
        }

        // While a turn is streaming, render the in-progress assistant
        // message + tool blocks so the user sees live progress.
        if let AppState::Streaming {
            partial,
            tool_blocks,
            ..
        } = &self.state
        {
            // Spinner glyph cycles based on a millisecond clock so the user
            // sees motion during the inevitable LLM round-trip.
            let frame_idx = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as usize / 100)
                .unwrap_or(0))
                % SPINNER_FRAMES.len();
            let spinner = SPINNER_FRAMES[frame_idx];
            let header = Line::from(vec![
                Span::styled(
                    format!("{spinner} "),
                    Style::default()
                        .fg(Color::Rgb(94, 184, 255))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("thinking…", Style::default().fg(Color::Rgb(107, 114, 128))),
            ]);
            items.push(ListItem::new(header));
            let lines =
                super::render::render_message_lines("assistant", partial, &[], tool_blocks, &theme);
            items.push(ListItem::new(lines));
        }

        let list = List::new(items).block(
            Block::default()
                .title(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        "Chat",
                        Style::default()
                            .fg(Color::Rgb(94, 184, 255))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ]))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Rgb(40, 70, 140))),
        );
        frame.render_widget(list, area);
    }

    /// Render the multi-line input area with the brand cyan accent.
    fn render_input(&self, frame: &mut ratatui::Frame, area: Rect) {
        let prefix = Span::styled(
            "▎ ",
            Style::default()
                .fg(Color::Rgb(94, 184, 255))
                .add_modifier(Modifier::BOLD),
        );
        let body = if self.context.input_buffer.is_empty() {
            Span::styled(
                "Type a message…  (Ctrl+Enter send · /help for commands · Ctrl+C exit)",
                Style::default().fg(Color::Rgb(107, 114, 128)),
            )
        } else {
            Span::raw(self.context.input_buffer.clone())
        };

        let input = Paragraph::new(Line::from(vec![prefix, body]))
            .block(
                Block::default()
                    .title(Line::from(vec![
                        Span::raw(" "),
                        Span::styled(
                            "$ you",
                            Style::default()
                                .fg(Color::Rgb(94, 184, 255))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" "),
                    ]))
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Rgb(94, 184, 255))),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(input, area);
    }

    /// Render the Hermes-style bottom status bar with model · usage · session age.
    fn render_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        let muted = Style::default().fg(Color::Rgb(107, 114, 128));
        let sky = Style::default().fg(Color::Rgb(94, 184, 255));
        let coral = Style::default()
            .fg(Color::Rgb(255, 123, 123))
            .add_modifier(Modifier::BOLD);

        let line = if let Some(ref err) = self.context.last_error {
            Line::from(vec![
                Span::styled(" ✗ ", coral),
                Span::styled(err.clone(), Style::default().fg(Color::Rgb(255, 123, 123))),
            ])
        } else {
            // Compact context-window meter — pretty-prints big numbers.
            let used = self.context.token_usage.total_tokens;
            let used_label = format_tokens(used);
            // Approximate context window from configured value if available.
            let window = self.context.context_window.unwrap_or(0);
            let window_label = if window > 0 {
                format!("/{}", format_tokens(window))
            } else {
                String::new()
            };
            let pct = if window > 0 {
                ((used as f64 / window as f64) * 100.0).round() as u32
            } else {
                0
            };

            // Session age in human time.
            let age_secs = self.context.started_at.elapsed().as_secs();
            let age_label = format_duration_short(age_secs);

            let mut spans = vec![
                Span::styled(" $ ", sky),
                Span::styled(
                    self.context.model.clone(),
                    Style::default()
                        .fg(Color::Rgb(94, 184, 255))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  │  ", muted),
                Span::styled(
                    format!("{used_label}{window_label}"),
                    Style::default().fg(Color::Rgb(126, 226, 179)),
                ),
                Span::styled(
                    if window > 0 {
                        format!("  {pct}%")
                    } else {
                        String::new()
                    },
                    muted,
                ),
                Span::styled("  │  ", muted),
                Span::styled(format!("{} msgs", self.context.messages.len()), muted),
                Span::styled("  │  ", muted),
                Span::styled(age_label, muted),
            ];
            append_autonomy_segment(&mut spans, self.context.autonomy_preset, muted);
            Line::from(spans)
        };

        let status = Paragraph::new(line);
        frame.render_widget(status, area);
    }
}

/// Append a `· <preset>` segment to the status bar when an active
/// preset is known. Colour-coded so the user can tell at a glance which
/// tier they're in: Off in coral (autonomy hot), Strict in muted-amber,
/// Manual in sky, Smart in mint (default-feel). When `preset` is `None`
/// (pre-onboarding, unreadable file), the segment is omitted entirely.
pub(crate) fn append_autonomy_segment(
    spans: &mut Vec<Span<'_>>,
    preset: Option<crate::approval::policy_writer::PolicyPreset>,
    muted: Style,
) {
    use crate::approval::policy_writer::PolicyPreset;
    let Some(p) = preset else {
        return;
    };
    let colour = match p {
        PolicyPreset::Off => Color::Rgb(255, 123, 123),
        PolicyPreset::Strict => Color::Rgb(241, 196, 15),
        PolicyPreset::Manual => Color::Rgb(94, 184, 255),
        PolicyPreset::Smart => Color::Rgb(126, 226, 179),
    };
    spans.push(Span::styled("  │  ", muted));
    spans.push(Span::styled(
        p.label().to_string(),
        Style::default().fg(colour).add_modifier(Modifier::BOLD),
    ));
}

/// One-line description for each approval preset, used by the Shift+Tab
/// confirmation toast and the `/autonomy` help text. Shorter than the
/// preset bundle's `description` field so it reads cleanly inline.
pub(crate) fn preset_blurb(preset: crate::approval::policy_writer::PolicyPreset) -> &'static str {
    use crate::approval::policy_writer::PolicyPreset;
    match preset {
        PolicyPreset::Manual => "every tool call prompts",
        PolicyPreset::Smart => "read-only auto, writes prompt",
        PolicyPreset::Strict => "deny by default, no prompts",
        PolicyPreset::Off => "no prompts — trusted env only",
    }
}

/// Spinner cycle used during streaming — Braille pattern matches the rest
/// of the brand's Unicode-forward look.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Provider → conventional env-var name. Matches what each provider's
/// auth path looks for in `apply_env_overrides` and the legacy CLI
/// instructions, so the "Missing API key" hint points the user at the
/// right thing instead of always saying OPENROUTER_API_KEY.
fn provider_env_var_name(provider: &str) -> String {
    match provider {
        "openrouter" => "OPENROUTER_API_KEY".into(),
        "anthropic" => "ANTHROPIC_API_KEY".into(),
        "openai" => "OPENAI_API_KEY".into(),
        "deepseek" => "DEEPSEEK_API_KEY".into(),
        "mistral" => "MISTRAL_API_KEY".into(),
        "xai" => "XAI_API_KEY".into(),
        "perplexity" => "PERPLEXITY_API_KEY".into(),
        "gemini" => "GEMINI_API_KEY".into(),
        "groq" => "GROQ_API_KEY".into(),
        "fireworks" => "FIREWORKS_API_KEY".into(),
        "together-ai" => "TOGETHER_API_KEY".into(),
        "nvidia" => "NVIDIA_API_KEY".into(),
        "vercel" => "VERCEL_AI_API_KEY".into(),
        "cloudflare" => "CLOUDFLARE_API_KEY".into(),
        "bedrock" => "AWS_ACCESS_KEY_ID".into(),
        "moonshot" | "moonshot-intl" => "MOONSHOT_API_KEY".into(),
        "glm" | "zai" => "GLM_API_KEY".into(),
        "minimax" => "MINIMAX_API_KEY".into(),
        "qwen" => "DASHSCOPE_API_KEY".into(),
        "qianfan" => "QIANFAN_API_KEY".into(),
        "cohere" => "COHERE_API_KEY".into(),
        "ollama" | "llamacpp" => "RANTAICLAW_API_KEY (no key needed)".into(),
        _ => "RANTAICLAW_API_KEY".into(),
    }
}

/// Recognise a handful of common agent error shapes and rewrite them as a
/// short, actionable chat message + a one-liner for the status bar.
/// Unknown errors fall through verbatim so we never lose information.
///
/// Returns `(chat_message_body, status_line)`.
fn format_agent_error(raw: &str, provider: &str) -> (String, String) {
    let lower = raw.to_lowercase();

    if lower.contains("api key not set") || lower.contains("api_key not set") {
        let env_name = provider_env_var_name(provider);
        let body = format!(
            "✗ Missing API key for `{provider}`.\n\
\n\
Set one of the following before sending a message:\n\
  • Export `{env_name}=…`\n\
  • Run `/setup provider` to save it to config\n\
  • Type `/quit`, then `rantaiclaw setup provider` for the guided flow"
        );
        return (
            body,
            format!("missing API key — set {env_name} or run /setup provider"),
        );
    }

    if lower.contains("429") || lower.contains("rate limit") || lower.contains("rate-limit") {
        let body = "⚠ Rate-limited by the provider.\n\n\
            Wait a few seconds and try again, or switch models with `/model`."
            .to_string();
        return (body, "provider rate limit hit".into());
    }

    if lower.contains("not a valid model id") || lower.contains("model not found") {
        let body = format!(
            "✗ Model unavailable.\n\n\
            The configured model isn't accepted by your provider. \
            Pick a different one with `/model <name>` or run \
            `rantaiclaw setup provider --force`.\n\n\
            Provider response: {}",
            first_meaningful_line(raw)
        );
        return (body, "model unavailable — see chat for details".into());
    }

    // Default: trim the verbose "Attempts: provider= ... attempt 1/3" tail
    // so the chat shows the human-readable cause first, then the rest as
    // dim context.
    let trimmed = compact_provider_error(raw);
    let status = first_meaningful_line(&trimmed);
    (format!("✗ {trimmed}"), status)
}

/// Pull the first non-trivial line out of a multi-line error blob.
fn first_meaningful_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("Attempts:"))
        .unwrap_or("")
        .to_string()
}

/// Reduce a "All providers/models failed. Attempts: …" blob to the most
/// useful line plus a one-liner attempts summary. Keeps the user oriented
/// without dumping the whole retry transcript.
fn compact_provider_error(s: &str) -> String {
    if !s.contains("All providers/models failed") {
        return s.to_string();
    }
    let attempts: Vec<&str> = s.lines().filter(|l| l.contains("attempt")).collect();
    let primary = attempts
        .first()
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| s.to_string());
    if attempts.len() > 1 {
        format!("{primary} (+{} more attempts)", attempts.len() - 1)
    } else {
        primary
    }
}

/// Route tracing output to a per-day log file under
/// `~/.rantaiclaw/logs/tui-YYYY-MM-DD.log` so warnings from the agent
/// path don't corrupt the TUI's alt-screen render. Best-effort: any
/// failure (no HOME, can't create directory, etc.) silently falls back
/// to whatever subscriber is already installed (which in TUI mode is
/// usually nothing — i.e. tracing becomes a no-op).
///
/// Idempotent: if a global subscriber is already set, this is a no-op.
/// That makes the function safe to call from multiple entry points.
fn install_tui_tracing() {
    use tracing_subscriber::EnvFilter;

    // Resolve the log path. Use the rantaiclaw root so it lives next to
    // the user's other state, not buried under XDG cache.
    let log_dir = crate::profile::paths::rantaiclaw_root().join("logs");
    if std::fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    let date = chrono::Utc::now().format("%Y-%m-%d");
    let log_path = log_dir.join(format!("tui-{date}.log"));

    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    else {
        return;
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt::Subscriber::builder()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .finish();

    // `set_global_default` returns Err if a subscriber is already set; we
    // happily swallow that — main()'s subscriber would have been less
    // appropriate for TUI mode anyway, but if it somehow ran first we
    // just leave it and accept the visual artifacts.
    let _ = tracing::subscriber::set_global_default(subscriber);
}

// ---------------------------------------------------------------------------
// Free-standing render helpers used by `TuiApp::render`. They take parameter
// references so the closure can call them while `render` holds a disjoint
// `&mut self.autocomplete` borrow.
// ---------------------------------------------------------------------------

fn render_header_pane(ctx: &TuiContext, frame: &mut ratatui::Frame, area: Rect) {
    let session_short = ctx.session_id_short();
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            "  Rantaiclaw  ",
            Style::default()
                .fg(Color::Rgb(4, 11, 46))
                .bg(Color::Rgb(94, 184, 255))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" v{}  ", env!("CARGO_PKG_VERSION")),
            Style::default()
                .fg(Color::Rgb(94, 184, 255))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("· session {} ", session_short),
            Style::default().fg(Color::Rgb(107, 114, 128)),
        ),
    ]));
    frame.render_widget(header, area);
}

fn render_chat_pane(state: &AppState, ctx: &TuiContext, frame: &mut ratatui::Frame, area: Rect) {
    let theme = super::render::RenderTheme::default();
    let mut items: Vec<ListItem> = Vec::with_capacity(ctx.messages.len() + 1);

    if ctx.messages.is_empty() && !matches!(state, AppState::Streaming { .. }) {
        for line in render_splash_lines(ctx, area.width) {
            items.push(ListItem::new(line));
        }
    }

    for msg in &ctx.messages {
        let persisted = msg
            .tool_calls
            .as_deref()
            .map(super::render::parse_persisted_tool_calls)
            .unwrap_or_default();
        let lines =
            super::render::render_message_lines(&msg.role, &msg.content, &persisted, &[], &theme);
        items.push(ListItem::new(lines));
    }

    if let AppState::Streaming {
        partial,
        tool_blocks,
        ..
    } = state
    {
        let frame_idx = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as usize / 100)
            .unwrap_or(0))
            % SPINNER_FRAMES.len();
        let spinner = SPINNER_FRAMES[frame_idx];
        let header = Line::from(vec![
            Span::styled(
                format!("{spinner} "),
                Style::default()
                    .fg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("thinking…", Style::default().fg(Color::Rgb(107, 114, 128))),
        ]);
        items.push(ListItem::new(header));
        let lines =
            super::render::render_message_lines("assistant", partial, &[], tool_blocks, &theme);
        items.push(ListItem::new(lines));
    }

    let list = List::new(items).block(
        Block::default()
            .title(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    "Chat",
                    Style::default()
                        .fg(Color::Rgb(94, 184, 255))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]))
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::Rgb(40, 70, 140))),
    );
    frame.render_widget(list, area);
}

/// Commit a block of `Line`s to the terminal scrollback above the inline
/// viewport. Splits across multiple `insert_before` calls so we never
/// exceed ratatui's max-insert-height (terminal height − viewport height),
/// which otherwise panics with `index outside of buffer`.
fn commit_lines_to_scrollback(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    lines: Vec<Line<'static>>,
    width: u16,
    term_height: u16,
) -> Result<()> {
    if lines.is_empty() || width == 0 {
        return Ok(());
    }
    // Max rows we can safely reserve in one insert_before call. ratatui
    // requires this to fit between the top of the screen and the top of
    // the inline viewport; leave a 1-row buffer for safety.
    let max_chunk: u16 = term_height
        .saturating_sub(INLINE_VIEWPORT_LINES)
        .saturating_sub(1)
        .max(1);

    let mut buf: Vec<Line<'static>> = Vec::new();
    let mut buf_rows: u16 = 0;

    let flush = |terminal: &mut Terminal<CrosstermBackend<Stdout>>,
                 chunk: &mut Vec<Line<'static>>,
                 rows: &mut u16|
     -> Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        let p = Paragraph::new(std::mem::take(chunk)).wrap(Wrap { trim: false });
        let height = (*rows).max(1);
        *rows = 0;
        terminal.insert_before(height, |b: &mut Buffer| {
            p.render(b.area, b);
        })?;
        Ok(())
    };

    for line in lines {
        // Estimate how many wrapped rows this line takes at `width`.
        // Use Paragraph::line_count for accuracy.
        let single = Paragraph::new(vec![line.clone()]).wrap(Wrap { trim: false });
        let row_count = single.line_count(width).max(1) as u16;

        // If a single line is taller than the chunk limit (extreme cases
        // like a 300-col line on a tall narrow terminal), cap it — we'd
        // rather lose tail rows than panic.
        let row_count = row_count.min(max_chunk);

        if buf_rows + row_count > max_chunk {
            flush(terminal, &mut buf, &mut buf_rows)?;
        }
        buf.push(line);
        buf_rows += row_count;
    }
    flush(terminal, &mut buf, &mut buf_rows)?;
    Ok(())
}

/// Render the live "stream preview" pane — sits above the input box and
/// shows the still-uncommitted tail of the assistant's reply as it
/// arrives. Empty when not streaming. The first row also shows a Braille
/// spinner so the user has motion to look at while bytes accumulate.
// Removed from the render pipeline in v0.6.50 — the status bar's
// WorkingState indicator covers the same ground. Kept around so it
// can be re-wired by flipping `STREAM_PREVIEW_LINES` back to `1` and
// adding the constraint + call to the layout in `render()`.
#[allow(dead_code)]
fn render_stream_preview_pane(
    state: &AppState,
    committed_chars: usize,
    frame: &mut ratatui::Frame,
    area: Rect,
) {
    let (partial, cancelling) = match state {
        AppState::Streaming {
            partial,
            cancelling,
            ..
        } => (partial.clone(), *cancelling),
        _ => return,
    };

    let frame_idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as usize / 100)
        .unwrap_or(0))
        % SPINNER_FRAMES.len();
    let spinner = SPINNER_FRAMES[frame_idx];
    let label = if cancelling {
        "cancelling…"
    } else if partial.len() == committed_chars {
        "thinking…"
    } else {
        "streaming…"
    };
    let muted = Style::default().fg(Color::Rgb(107, 114, 128));
    let sky = Style::default()
        .fg(Color::Rgb(94, 184, 255))
        .add_modifier(Modifier::BOLD);

    // Single-row layout: `[spinner] streaming…   <last few words of the
    // in-progress line>`. Completed lines flow into scrollback so the
    // preview never grows beyond one row — keeps the viewport tight and
    // matches Hermes / Claude Code feel.
    let safe_committed = committed_chars.min(partial.len());
    let tail = &partial[safe_committed..];
    let last_line = tail.rsplit('\n').next().unwrap_or("");
    let snippet = if last_line.chars().count() > 60 {
        let total = last_line.chars().count();
        let skip = total.saturating_sub(58);
        let suffix: String = last_line.chars().skip(skip).collect();
        format!("…{suffix}")
    } else {
        last_line.to_string()
    };

    let mut spans = vec![
        Span::styled(format!("  {spinner} "), sky),
        Span::styled(label.to_string(), muted),
    ];
    if snippet.trim().is_empty() {
        spans.push(Span::styled("    Ctrl+C to cancel".to_string(), muted));
    } else {
        spans.push(Span::styled("    ".to_string(), muted));
        spans.push(Span::styled(
            snippet,
            Style::default().fg(Color::Rgb(180, 200, 220)),
        ));
    }
    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    frame.render_widget(paragraph, area);
}

/// Render the boxed approval prompt that replaces the input row while
/// a `PendingRequest` is open. Designed to match Claude Code's prompt
/// style: amber border (caution), basename in bold, full command in a
/// muted block with newlines preserved, action chips with the hotkey
/// letter highlighted.
///
/// Layout (4 rows total, same height as the input box it replaces):
///
/// ```text
/// ╭ 🔒 Approval needed · cd ────────────────────────────╮
/// │ cd /home/.../skills/stocks && .venv/bin/python …   │
/// │                                                     │
/// │ [Y] yes once   [A] always   [N] no   [Esc] deny    │
/// ╰─────────────────────────────────────────────────────╯
/// ```
fn render_approval_pane(
    req: &crate::security::PendingRequest,
    frame: &mut ratatui::Frame,
    area: Rect,
) {
    let amber = Color::Rgb(241, 196, 15);
    let muted = Color::Rgb(150, 150, 150);
    let key_bg = Color::Rgb(241, 196, 15);
    let key_fg = Color::Rgb(20, 20, 20);

    // Compress the command preview to a single line — multi-line
    // heredocs (e.g. the stocks skill's `python3 - <<'PY' ... PY`)
    // blow out the 4-row pane and bury the action chips. The full
    // command was already shown in the system-message scrollback line
    // when the request landed, so this preview is just enough context
    // for the user to decide.
    let one_line = req.full_command.replace('\n', " ⏎ ");
    let preview = if one_line.chars().count() > 200 {
        let head: String = one_line.chars().take(197).collect();
        format!("{head}…")
    } else {
        one_line
    };

    fn chip(
        key: &str,
        label: &str,
        key_bg: Color,
        key_fg: Color,
        muted: Color,
    ) -> Vec<Span<'static>> {
        vec![
            Span::styled(
                format!(" {key} "),
                Style::default()
                    .bg(key_bg)
                    .fg(key_fg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {label}   "), Style::default().fg(muted)),
        ]
    }

    let mut chip_spans: Vec<Span<'static>> = Vec::new();
    chip_spans.extend(chip("Y", "yes once", key_bg, key_fg, muted));
    chip_spans.extend(chip("A", "always (persist)", key_bg, key_fg, muted));
    chip_spans.extend(chip("N", "no", key_bg, key_fg, muted));
    chip_spans.extend(chip("Esc", "deny", key_bg, key_fg, muted));

    let body = vec![
        Line::from(vec![
            Span::styled(" ", Style::default()),
            Span::styled(preview, Style::default().fg(Color::Rgb(220, 220, 220))),
        ]),
        Line::from(chip_spans),
    ];

    let title = Line::from(vec![
        Span::raw(" "),
        Span::styled(
            "🔒 Approval needed",
            Style::default().fg(amber).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", Style::default().fg(muted)),
        Span::styled(
            req.basename.clone(),
            Style::default()
                .fg(Color::Rgb(255, 255, 255))
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ]);

    let para = Paragraph::new(body)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(amber)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

#[cfg(test)]
mod composer_body_tests {
    use super::composer_body;

    /// The bug this guards: `Span::raw(input_buffer)` handed ratatui a span
    /// whose content held `\n`. Ratatui writes span content verbatim, so the
    /// newline moved the terminal cursor out of the input box and the border
    /// was overwritten — reproduced with any 2-line paste.
    #[test]
    fn no_span_carries_a_raw_newline() {
        for buffer in ["a\nb", "one\ntwo\nthree", "trailing\n", "\nleading"] {
            let text = composer_body(buffer);
            for line in &text.lines {
                for span in &line.spans {
                    assert!(
                        !span.content.contains('\n'),
                        "buffer {buffer:?} produced a span carrying a raw newline: {:?}",
                        span.content
                    );
                }
            }
        }
    }

    /// A terminal transmits pasted line breaks as CR, not LF: a known-good
    /// crossterm probe fed `"AAA\nBBB"` through tmux's bracketed paste
    /// received `Event::Paste("AAA\rBBB")`. A bare `\r` in a `Span` returns
    /// the real cursor to column 0, so the rest of the paste overwrites the
    /// composer's left border — this, not `\n`, is what shredded the box.
    /// Guard every line-break form a paste can carry.
    #[test]
    fn no_span_carries_any_bare_carriage_return() {
        for buffer in [
            "a\rb",
            "a\r\nb",
            "one\rtwo\rthree",
            "trailing\r",
            "\rleading",
        ] {
            let text = composer_body(buffer);
            for line in &text.lines {
                for span in &line.spans {
                    assert!(
                        !span.content.contains('\r') && !span.content.contains('\n'),
                        "buffer {buffer:?} produced a span carrying a raw line break: {:?}",
                        span.content
                    );
                }
            }
        }
    }

    /// CR, LF and CRLF must all break exactly one line — CRLF must not
    /// produce a phantom empty line between the two halves.
    #[test]
    fn every_line_break_form_splits_the_same_way() {
        assert_eq!(composer_body("a\nb").lines.len(), 2, "LF");
        assert_eq!(composer_body("a\rb").lines.len(), 2, "CR");
        assert_eq!(composer_body("a\r\nb").lines.len(), 2, "CRLF");
    }

    #[test]
    fn splits_one_line_per_logical_line() {
        assert_eq!(composer_body("a\nb\nc").lines.len(), 3);
        assert_eq!(composer_body("single").lines.len(), 1);
    }

    /// A trailing newline is where the cursor sits after `Ctrl+J`; dropping
    /// the empty final line (as `str::lines` would) hides it.
    #[test]
    fn trailing_newline_keeps_its_empty_final_line() {
        let text = composer_body("body\n");
        assert_eq!(text.lines.len(), 2);
        let last: String = text.lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(last.trim(), "");
    }

    #[test]
    fn empty_buffer_renders_the_placeholder_on_one_line() {
        let text = composer_body("");
        assert_eq!(text.lines.len(), 1);
        let rendered: String = text.lines[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(rendered.contains("Type a message"), "got {rendered:?}");
    }

    /// Every logical line must survive verbatim — the fix must not drop or
    /// reorder content while splitting.
    #[test]
    fn content_survives_the_split_verbatim() {
        let text = composer_body("first\nsecond\nthird");
        let bodies: Vec<String> = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim_start_matches(['▎', ' '])
                    .to_string()
            })
            .collect();
        assert_eq!(bodies, vec!["first", "second", "third"]);
    }
}

#[cfg(test)]
mod composer_caret_tests {
    use super::{composer_caret_cell, composer_scroll};

    #[test]
    fn caret_starts_after_the_prefix() {
        assert_eq!(composer_caret_cell("", 0, 40), (0, 2));
    }

    #[test]
    fn caret_advances_along_the_first_row() {
        assert_eq!(composer_caret_cell("abc", 3, 40), (0, 5)); // 2 prefix + 3
    }

    #[test]
    fn a_newline_returns_the_caret_to_column_zero_on_the_next_row() {
        assert_eq!(composer_caret_cell("ab\ncd", 5, 40), (1, 2));
        assert_eq!(composer_caret_cell("ab\n", 3, 40), (1, 0));
    }

    #[test]
    fn caret_wraps_when_it_runs_past_the_inner_width() {
        // width 10, prefix 2 → 8 chars fit on row 0.
        assert_eq!(composer_caret_cell(&"x".repeat(8), 8, 10), (1, 0));
    }

    /// The view must not move until the caret would leave the window — the
    /// composer should sit still while the user types the first rows.
    #[test]
    fn no_scroll_while_the_caret_fits() {
        for caret_row in 0..3 {
            assert_eq!(composer_scroll(caret_row, 3), 0, "row {caret_row}");
        }
    }

    /// The bug: the caret row was `min`-clamped to the last visible row, so
    /// past 2 rows of input the caret froze on the bottom line and lied about
    /// where typing was going. Scrolling by exactly the overflow keeps it
    /// honest.
    #[test]
    fn scrolls_exactly_enough_to_keep_the_caret_visible() {
        assert_eq!(composer_scroll(3, 3), 1);
        assert_eq!(composer_scroll(9, 2), 8);
    }

    /// The invariant the render relies on: caret_row - scroll is always a
    /// valid row inside the window, for any buffer and any window height.
    #[test]
    fn caret_always_lands_inside_the_window() {
        for inner_h in 1..6u16 {
            for caret_row in 0..40u16 {
                let scroll = composer_scroll(caret_row, inner_h);
                let visible = caret_row.saturating_sub(scroll);
                assert!(
                    visible < inner_h,
                    "caret_row {caret_row} inner_h {inner_h} scroll {scroll} → {visible}"
                );
            }
        }
    }
}

/// Collapse every line-break form to `\n`.
///
/// The composer's buffer invariant is "line breaks are `\n`". Terminals do
/// not honour it for free: a bracketed paste transmits breaks as CR, so
/// pasting `"AAA\nBBB"` delivers `Event::Paste("AAA\rBBB")` (verified against
/// a standalone crossterm probe through tmux). A bare `\r` reaching a ratatui
/// `Span` returns the physical cursor to column 0 and the rest of the paste
/// overwrites the input box's border; a `\r` in the buffer also desyncs the
/// caret walker, which only counts `\n`.
///
/// CRLF collapses to one break, never two.
fn normalize_line_breaks(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Caret cell for `cursor_pos` as `(row, col)` inside the composer's inner
/// area, before any scroll is applied.
///
/// Char-cell wrap model: each char (including the leading "▎ " prefix)
/// consumes one terminal cell. Exact for ASCII, approximate for full-width
/// and combining glyphs, and it hard-breaks at `inner_w` where ratatui's
/// `Wrap` breaks on word boundaries — a pre-existing skew this preserves
/// rather than fixes.
fn composer_caret_cell(buffer: &str, cursor_pos: usize, inner_w: u16) -> (u16, u16) {
    let prefix_cells: u16 = 2; // "▎ "
    let mut col: u16 = prefix_cells.min(inner_w.saturating_sub(1));
    let mut row: u16 = 0;
    for ch in buffer.chars().take(cursor_pos) {
        if ch == '\n' {
            col = 0;
            row = row.saturating_add(1);
        } else {
            col = col.saturating_add(1);
            if col >= inner_w {
                col = 0;
                row = row.saturating_add(1);
            }
        }
    }
    (row, col.min(inner_w.saturating_sub(1)))
}

/// Vertical scroll that keeps `caret_row` inside an `inner_h`-row window.
///
/// Zero until the caret would fall off the bottom, then exactly the overflow
/// — so the box stays put while the user types the first rows and then
/// follows the caret one row at a time.
fn composer_scroll(caret_row: u16, inner_h: u16) -> u16 {
    caret_row.saturating_sub(inner_h.saturating_sub(1))
}

/// Build the composer's renderable text.
///
/// A ratatui `Span` must stay newline-free: its content is written to the
/// terminal verbatim, so an embedded `\n` moves the real cursor out of the
/// widget and shreds the surrounding border. `Ctrl+J` (`insert_char_at_cursor`)
/// and bracketed paste both put literal `\n` in `input_buffer`, so split into
/// one `Line` per logical line here rather than handing the raw buffer to
/// `Span::raw`.
fn composer_body(input_buffer: &str) -> Text<'static> {
    let prefix = Span::styled(
        "▎ ",
        Style::default()
            .fg(Color::Rgb(94, 184, 255))
            .add_modifier(Modifier::BOLD),
    );

    if input_buffer.is_empty() {
        return Text::from(Line::from(vec![
            prefix,
            Span::styled(
                "Type a message…  (Enter sends · /help for commands · Ctrl+J newline · Ctrl+C exit)",
                Style::default().fg(Color::Rgb(107, 114, 128)),
            ),
        ]));
    }

    // `split('\n')` (not `lines()`) so a trailing newline keeps its empty
    // final line — the cursor sits there and the user must see it.
    //
    // Continuation lines carry no indent: ratatui's own soft-wrap returns to
    // column 0, and the caret walker below likewise resets `col` to 0 after a
    // '\n'. Indenting here would make hard-broken lines disagree with both.
    //
    // Normalize defensively even though `Event::Paste` already does: this is
    // the last hop before the bytes reach the terminal, and a single stray
    // '\r' here is not a cosmetic bug — it returns the real cursor to column
    // 0 and the rest of the line eats the border.
    let normalized = normalize_line_breaks(input_buffer);
    let lines: Vec<Line<'static>> = normalized
        .split('\n')
        .enumerate()
        .map(|(i, logical)| {
            let text = Span::raw(logical.to_string());
            if i == 0 {
                Line::from(vec![prefix.clone(), text])
            } else {
                Line::from(text)
            }
        })
        .collect();

    Text::from(lines)
}

fn render_input_pane(ctx: &TuiContext, frame: &mut ratatui::Frame, area: Rect) {
    let inner_x = area.x.saturating_add(1);
    let inner_y = area.y.saturating_add(1);
    let inner_w = area.width.saturating_sub(2).max(1);
    let inner_h = area.height.saturating_sub(2).max(1);

    // Scroll the box to follow the caret. Without this the Paragraph renders
    // from row 0 forever and everything past `inner_h` is silently clipped —
    // a long paste looked like it had been swallowed even though the whole
    // buffer was there and submitted intact.
    let (caret_row, caret_col) = composer_caret_cell(&ctx.input_buffer, ctx.cursor_pos, inner_w);
    let scroll = composer_scroll(caret_row, inner_h);

    let input = Paragraph::new(composer_body(&ctx.input_buffer))
        .scroll((scroll, 0))
        .block(
            Block::default()
                .title(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        "$ you",
                        Style::default()
                            .fg(Color::Rgb(94, 184, 255))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ]))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Rgb(94, 184, 255))),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(input, area);

    // Position the terminal cursor at the insertion point so the user
    // can see where they are typing and so Left/Right/Home/End feedback
    // is visible. ratatui hides the cursor unless `set_cursor_position`
    // is called on every frame — without it the input box looks frozen
    // even though the buffer is actually updating, which was reported
    // as "characters don't appear until I press Enter".
    frame.set_cursor_position((inner_x + caret_col, inner_y + caret_row - scroll));
}

fn render_status_pane(ctx: &TuiContext, state: &AppState, frame: &mut ratatui::Frame, area: Rect) {
    let muted = Style::default().fg(Color::Rgb(107, 114, 128));
    let sky = Style::default().fg(Color::Rgb(94, 184, 255));
    let coral = Style::default()
        .fg(Color::Rgb(255, 123, 123))
        .add_modifier(Modifier::BOLD);

    // While the agent is streaming a turn, the status line is more
    // useful as a "what is happening right now" indicator than as a
    // model/token meter. We replace the whole line with the
    // wall-clock-driven working indicator. When the turn finishes,
    // the line snaps back to the normal token/age view.
    if let AppState::Streaming {
        tool_blocks,
        cancelling,
        turn_started_at,
        ..
    } = state
    {
        use crate::tui::widgets::working_indicator::{render as render_indicator, WorkingState};
        let now = std::time::Instant::now();
        let indicator_state = if *cancelling {
            WorkingState::Cancelling
        } else if let Some(current) = tool_blocks.iter().rev().find(|b| b.result.is_none()) {
            WorkingState::Tool {
                name: current.name.as_str(),
                tool_started: current.started_at,
            }
        } else {
            WorkingState::Thinking {
                turn_started: *turn_started_at,
            }
        };
        let mut line = render_indicator(&indicator_state, now);
        // Append "+ N queued" so the user can see follow-up submissions
        // are stacked behind the active turn. Without this, the only
        // signal that a queued submit landed is the input buffer
        // clearing — easy to miss, easy to double-submit.
        if ctx.queued_turns > 0 {
            line.spans.push(Span::styled("  ·  ", muted));
            line.spans.push(Span::styled(
                format!("+{} queued", ctx.queued_turns),
                Style::default().fg(Color::Rgb(241, 196, 15)),
            ));
        }
        frame.render_widget(Paragraph::new(line), area);
        return;
    }

    let line = if let Some(ref err) = ctx.last_error {
        Line::from(vec![
            Span::styled(" ✗ ", coral),
            Span::styled(err.clone(), Style::default().fg(Color::Rgb(255, 123, 123))),
        ])
    } else {
        let used = ctx.token_usage.total_tokens;
        let used_label = format_tokens(used);
        let age_secs = ctx.started_at.elapsed().as_secs();
        let age_label = format_duration_short(age_secs);

        let mut spans = vec![
            Span::styled(" $ ", sky),
            Span::styled(
                ctx.model.clone(),
                Style::default()
                    .fg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  │  ", muted),
            Span::styled(used_label, Style::default().fg(Color::Rgb(126, 226, 179))),
            Span::styled("  │  ", muted),
            Span::styled(format!("{} msgs", ctx.messages.len()), muted),
            Span::styled("  │  ", muted),
            Span::styled(age_label, muted),
        ];
        append_autonomy_segment(&mut spans, ctx.autonomy_preset, muted);
        Line::from(spans)
    };

    let status = Paragraph::new(line);
    frame.render_widget(status, area);
}

/// Rantai logo mascot, drawn as Unicode block + ASCII hash glyphs that
/// `mascot_color()` colours per-region: head silhouette in deep navy
/// (`█`), two accent squares in light blue (`#`). On terminals with
/// truecolour support the splash matches the brand palette; terminals
/// that lack it downmix to the nearest 256-colour cell, and the
/// underlying glyphs still convey the silhouette even when colour is
/// dropped entirely (cat the asset to see the raw shape).
const MASCOT_ART: &str = include_str!("assets/mascot_ascii.txt");

/// Width of the mascot column (in display cells). Must be at least as
/// wide as the longest row of `MASCOT_ART` so the right-pane
/// stays aligned.
const MASCOT_WIDTH: usize = 39;

/// Hard ceiling on the right-pane width, in chars. Used by `wrap_csv`
/// and `wrap_text`. Kept conservative so the stitched splash row
/// (mascot + separator + right text) stays well under the width of a
/// typical 100-col terminal, preventing the right-pane content from
/// reflowing onto the mascot's row.
const MAX_RIGHT_WIDTH: usize = 50;

/// Map a single glyph from the mascot ASCII art to its RGB tint. Returning
/// `None` means "render this cell as a plain space with no styling" —
/// the splash uses that for whitespace so the art blends with whatever
/// background the terminal happens to be using.
///
/// Two glyph groups carry the Rantai logo: `@`/`$` are the diagonal
/// slashes in sky blue (centre), and `;`/`+` are the two bracket frames
/// in navy. The pair reuses the RANTAICLAW figlet gradient endpoints so
/// logo and wordmark share one palette. All four are plain ASCII so
/// terminals without truecolour downmix the colour but the silhouette
/// stays intact.
fn mascot_color(ch: char) -> Option<Color> {
    match ch {
        // Diagonal slashes (centre) — sky, the top of the RANTAICLAW
        // figlet gradient so the logo and wordmark share one palette.
        '@' | '$' => Some(Color::Rgb(94, 184, 255)),
        // Bracket frames — a lifted navy (royal blue), brighter than the
        // figlet's darkest navy but still clearly deeper than the sky
        // slashes so the two regions stay distinct.
        ';' | '+' => Some(Color::Rgb(60, 110, 200)),
        _ => None,
    }
}

/// Convert one row of `MASCOT_ART` into a `Line` of styled spans,
/// batching contiguous same-colour runs into a single span to keep the
/// span count manageable for ratatui.
fn mascot_row(row: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Color> = None;
    let mut started = false;

    let flush = |spans: &mut Vec<Span<'static>>, buf: &mut String, color: Option<Color>| {
        if buf.is_empty() {
            return;
        }
        let span = match color {
            Some(c) => Span::styled(
                std::mem::take(buf),
                Style::default().fg(c).add_modifier(Modifier::BOLD),
            ),
            None => Span::raw(std::mem::take(buf)),
        };
        spans.push(span);
    };

    for ch in row.chars() {
        let color = mascot_color(ch);
        if !started {
            cur = color;
            started = true;
        } else if color != cur {
            flush(&mut spans, &mut buf, cur);
            cur = color;
        }
        buf.push(ch);
    }
    flush(&mut spans, &mut buf, cur);
    Line::from(spans)
}

/// Render the empty-chat splash. Adapts at render time to the
/// terminal width supplied by the caller:
///
/// * `width ≥ 80` — full RANTAICLAW figlet at top
/// * `42 ≤ width < 80` — small figlet
/// * `width < 42` — plain bold wordmark
///
/// * `width ≥ MASCOT_WIDTH + 2 + MIN_RIGHT_WIDTH` — side-by-side
///   mascot (left) + info (right)
/// * otherwise — stacked: mascot above info
///
/// Right-pane copy wraps to `(width − mascot − sep)` (capped at
/// `MAX_RIGHT_WIDTH`) so it never bleeds onto the mascot's row.
/// Because ratatui calls this once per frame with the current
/// `area.width`, the splash re-flows live as the terminal resizes.
fn render_splash_lines(ctx: &TuiContext, area_width: u16) -> Vec<Line<'static>> {
    let muted = Color::Rgb(107, 114, 128);
    let sky = Color::Rgb(94, 184, 255);
    let gold = Color::Rgb(234, 179, 8);

    let avail = area_width as usize;

    // Minimum useful width for the right column when sitting beside the
    // mascot. Below this we stack instead so neither pane is starved.
    const MIN_RIGHT_WIDTH: usize = 28;

    let side_by_side = avail >= MASCOT_WIDTH + 2 + MIN_RIGHT_WIDTH;
    // Skip the mascot entirely on terminals too narrow to hold its
    // canvas — at that point every row would wrap into two visual
    // rows and the silhouette becomes unreadable. The title figlet
    // (or fallback wordmark) still conveys brand identity.
    let show_mascot = avail >= MASCOT_WIDTH;
    let right_width = if side_by_side {
        avail
            .saturating_sub(MASCOT_WIDTH + 2)
            .min(MAX_RIGHT_WIDTH)
            .max(MIN_RIGHT_WIDTH)
    } else {
        avail.min(MAX_RIGHT_WIDTH).max(20)
    };
    let inner_wrap = right_width.saturating_sub(2).max(16);

    // ── Right-pane content ────────────────────────────────────────────
    let mut right: Vec<Line<'static>> = Vec::new();

    right.push(Line::from(vec![
        Span::styled(
            "Rantaiclaw",
            Style::default().fg(sky).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(muted),
        ),
    ]));
    right.push(Line::from(""));

    right.push(Line::from(Span::styled(
        "Available Channels",
        Style::default().fg(gold).add_modifier(Modifier::BOLD),
    )));
    let channels: Vec<String> = ctx
        .channels_summary
        .iter()
        .filter(|(_, configured)| *configured)
        .map(|(name, _)| name.clone())
        .collect();
    if channels.is_empty() {
        for line in wrap_text(
            "(none configured — run `/setup channels` to enable transports)",
            inner_wrap,
        ) {
            right.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(line, Style::default().fg(muted)),
            ]));
        }
    } else {
        for line in wrap_csv(&channels, inner_wrap) {
            right.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(line, Style::default().fg(sky)),
            ]));
        }
    }
    right.push(Line::from(""));

    right.push(Line::from(Span::styled(
        "Available Skills",
        Style::default().fg(gold).add_modifier(Modifier::BOLD),
    )));
    let skills: Vec<String> = ctx
        .available_skills
        .iter()
        .map(|s| s.name.clone())
        .collect();
    if skills.is_empty() {
        for line in wrap_text(
            "(none installed — run `/setup skills` or `/skill install <name>`)",
            inner_wrap,
        ) {
            right.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(line, Style::default().fg(muted)),
            ]));
        }
    } else {
        for line in wrap_csv(&skills, inner_wrap) {
            right.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(line, Style::default().fg(sky)),
            ]));
        }
    }
    right.push(Line::from(""));

    for line in wrap_text("Type a message or /help for commands.", right_width) {
        right.push(Line::from(Span::styled(line, Style::default().fg(muted))));
    }

    // ── Left-pane mascot ──────────────────────────────────────────────
    let mut left: Vec<Line<'static>> = Vec::new();
    for row in MASCOT_ART.lines() {
        left.push(mascot_row(row));
    }

    // ── Top section: brand wordmark (figlet or plain text) ────────────
    let figlet_palette = [
        Color::Rgb(94, 184, 255),  // sky
        Color::Rgb(94, 184, 255),  // sky
        Color::Rgb(59, 140, 255),  // blue
        Color::Rgb(59, 140, 255),  // blue
        Color::Rgb(40, 70, 140),   // navy
        Color::Rgb(107, 114, 128), // muted
    ];
    let banner: &'static str = if avail >= 80 {
        include_str!("../onboard/assets/banner_full.txt")
    } else if avail >= 42 {
        include_str!("../onboard/assets/banner_small.txt")
    } else {
        ""
    };

    let mut out: Vec<Line<'static>> = Vec::new();
    out.push(Line::from(""));
    if banner.is_empty() {
        // Fallback wordmark for very narrow terminals.
        out.push(Line::from(Span::styled(
            "RANTAICLAW",
            Style::default()
                .fg(figlet_palette[0])
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        for (i, line) in banner.lines().enumerate() {
            let color = figlet_palette[i.min(figlet_palette.len() - 1)];
            out.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
        }
    }
    out.push(Line::from(""));

    // ── Body: side-by-side stitch OR stacked rendering ────────────────
    if side_by_side {
        let rows = left.len().max(right.len());
        for i in 0..rows {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if let Some(line) = left.get(i) {
                spans.extend(line.spans.iter().cloned());
                let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
                if used < MASCOT_WIDTH {
                    spans.push(Span::raw(" ".repeat(MASCOT_WIDTH - used)));
                }
            } else {
                spans.push(Span::raw(" ".repeat(MASCOT_WIDTH)));
            }
            spans.push(Span::raw("  "));
            if let Some(line) = right.get(i) {
                spans.extend(line.spans.iter().cloned());
            }
            out.push(Line::from(spans));
        }
    } else {
        // Stacked: mascot first (if the canvas fits at all), then a
        // blank, then the info pane. Below `MASCOT_WIDTH` we drop the
        // mascot entirely — the figlet/text wordmark on top already
        // carries the brand and a half-wrapped crab just adds noise.
        if show_mascot {
            for line in left {
                out.push(line);
            }
            out.push(Line::from(""));
        }
        for line in right {
            out.push(line);
        }
    }
    out.push(Line::from(""));
    out
}

/// Word-wrap `text` at spaces so no output line exceeds `width`
/// columns. Long tokens that themselves overflow are still emitted on
/// their own line (truncation would lose information; the caller can
/// decide whether to clip). Returns at least one row even when `text`
/// is empty.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            out.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Join `items` with `", "` and word-wrap so no output line exceeds
/// `width` columns. Returns at least one row even when `items` is empty.
fn wrap_csv(items: &[String], width: usize) -> Vec<String> {
    if items.is_empty() {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, item) in items.iter().enumerate() {
        let token = if i + 1 == items.len() {
            item.clone()
        } else {
            format!("{item}, ")
        };
        if !cur.is_empty() && cur.chars().count() + token.chars().count() > width {
            out.push(std::mem::take(&mut cur));
        }
        cur.push_str(&token);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Format token counts with K / M suffixes so the status bar stays compact.
fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Render tool-call arguments as a compact single-line summary for the
/// inline scrollback log line. Picks the most informative-looking
/// scalar field (`command`, `path`, `query`, etc.) and truncates so
/// the whole line stays readable. Multi-field objects degrade to
/// `<N args>` rather than smearing across the screen.
#[cfg(test)]
mod compact_args_for_log_tests {
    use super::compact_args_for_log;

    /// Tool arguments are model-supplied and routinely non-ASCII — a CJK
    /// search `query`, an emoji in a `path`. Cropping them with a byte
    /// slice panicked whenever the 50-byte mark landed inside a multi-byte
    /// char, so formatting one log line killed the whole TUI.
    ///
    /// `crate::agent::events::truncate_preview` already guards the same
    /// class (see `tool_output_preview_respects_char_boundaries`), and the
    /// sibling `render::truncate_preview` crops by chars; this site was the
    /// one that reached for `&s[..n]`.
    #[test]
    fn does_not_panic_when_the_crop_lands_inside_a_multibyte_char() {
        // '世' occupies bytes 48..51, so the byte-50 crop is mid-codepoint.
        let args = serde_json::json!({ "query": format!("{}世界", "a".repeat(48)) });
        let out = compact_args_for_log(&args);
        assert!(out.starts_with("query="), "got {out:?}");
    }

    /// A crop must never split a char, whatever the multi-byte width.
    #[test]
    fn crops_every_multibyte_width_safely() {
        for filler in ['é', '世', '🦀'] {
            for pad in 40..60 {
                let args = serde_json::json!({
                    "path": format!("{}{}", "a".repeat(pad), filler.to_string().repeat(20)),
                });
                let out = compact_args_for_log(&args);
                assert!(
                    out.starts_with("path="),
                    "pad {pad} filler {filler}: {out:?}"
                );
            }
        }
    }

    #[test]
    fn long_values_are_cropped_with_an_ellipsis() {
        let args = serde_json::json!({ "command": "b".repeat(200) });
        let out = compact_args_for_log(&args);
        assert!(out.contains('…'), "got {out:?}");
    }

    #[test]
    fn short_values_are_kept_whole() {
        let args = serde_json::json!({ "command": "ls -la" });
        assert_eq!(compact_args_for_log(&args), r#"command="ls -la""#);
    }

    #[test]
    fn unpreferred_keys_fall_back_to_a_count() {
        let args = serde_json::json!({ "alpha": "x", "beta": "y" });
        assert_eq!(compact_args_for_log(&args), "<2 args>");
    }

    #[test]
    fn empty_and_non_object_args_render_nothing() {
        assert_eq!(compact_args_for_log(&serde_json::json!({})), "");
        assert_eq!(compact_args_for_log(&serde_json::json!("bare")), "");
    }
}

fn compact_args_for_log(args: &serde_json::Value) -> String {
    const PREFERRED_KEYS: &[&str] = &[
        "command",
        "cmd",
        "path",
        "file_path",
        "query",
        "url",
        "name",
        "key",
        "pattern",
    ];
    const MAX_LEN: usize = 50;
    if let serde_json::Value::Object(map) = args {
        if map.is_empty() {
            return String::new();
        }
        for k in PREFERRED_KEYS {
            if let Some(v) = map.get(*k) {
                if let Some(s) = v.as_str() {
                    // Crop by chars, not bytes: tool args are model-supplied
                    // and a byte crop panics when it lands mid-codepoint.
                    let cropped = super::render::truncate_preview(s.trim(), MAX_LEN);
                    return format!("{k}={cropped:?}");
                }
            }
        }
        // No preferred-key match; fall back to a count-only summary.
        let n = map.len();
        return format!("<{n} arg{}>", if n == 1 { "" } else { "s" });
    }
    String::new()
}

/// Render the modal `/help`-style overlay over the chat area. Layout
/// mirrors Claude Code's:
///   ┌ Rantaiclaw v0.5.0  [general]  commands ───────────┐
///   │                                                   │
///   │ <body of active tab>                              │
///   │                                                   │
///   │                                Esc to close       │
///   └───────────────────────────────────────────────────┘
fn render_overlay_pane(
    content: &super::commands::OverlayContent,
    frame: &mut ratatui::Frame,
    area: Rect,
) {
    use ratatui::widgets::{Clear, Paragraph};

    if area.height < 5 || area.width < 30 {
        // Terminal too small for the overlay; fall back to silently
        // skipping the panel — the user can still see the chat behind it
        // and dismiss with Esc.
        return;
    }

    // Center the panel — leave a 2-col gutter on each side.
    let inner_w = area.width.saturating_sub(2);
    let inner_h = area.height.saturating_sub(2);
    let panel_area = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: inner_w,
        height: inner_h,
    };

    // Draw a clean opaque background so chat content underneath doesn't
    // bleed through.
    frame.render_widget(Clear, panel_area);

    let sky = Color::Rgb(94, 184, 255);
    let blue = Color::Rgb(59, 140, 255);
    let muted = Color::Rgb(107, 114, 128);
    let active_bg = Color::Rgb(94, 184, 255);
    let frame_color = Color::Rgb(40, 70, 140);

    // Title spans the full width. Active tab gets a sky-blue chip; inactive
    // tabs are muted.
    let mut title_spans: Vec<Span<'static>> = vec![
        Span::raw(" "),
        Span::styled(
            content.title.clone(),
            Style::default().fg(sky).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    for (i, tab) in content.tabs.iter().enumerate() {
        if i == content.active_tab {
            title_spans.push(Span::styled(
                format!(" {} ", tab.label),
                Style::default()
                    .fg(Color::Rgb(4, 11, 46))
                    .bg(active_bg)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            title_spans.push(Span::styled(
                format!(" {} ", tab.label),
                Style::default().fg(muted),
            ));
        }
        title_spans.push(Span::raw(" "));
    }

    let block = Block::default()
        .title(Line::from(title_spans))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(frame_color));

    let body_lines: Vec<Line> = content
        .tabs
        .get(content.active_tab)
        .map(|t| {
            let mut lines: Vec<Line> = Vec::with_capacity(t.body.len() + 2);
            for raw in &t.body {
                if raw.is_empty() {
                    lines.push(Line::from(""));
                    continue;
                }
                // Section header heuristic: line that doesn't start with
                // whitespace and ends without colon (or is short) — bolden
                // it as a category label.
                let is_section = !raw.starts_with(' ')
                    && !raw.contains("://")
                    && raw.split_whitespace().count() <= 4;
                if is_section {
                    lines.push(Line::from(Span::styled(
                        raw.clone(),
                        Style::default().fg(blue).add_modifier(Modifier::BOLD),
                    )));
                } else {
                    // Inside the body lines, bullet rows are coloured to make
                    // command names + key bindings pop without overdoing it.
                    lines.push(highlight_help_line(raw, sky, muted));
                }
            }
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    "  Esc",
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" to close · ", Style::default().fg(muted)),
                Span::styled(
                    "Tab / ← →",
                    Style::default().fg(sky).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" to switch tabs", Style::default().fg(muted)),
            ]));
            lines
        })
        .unwrap_or_default();

    let body = Paragraph::new(body_lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(body, panel_area);
}

/// Color the leading token of a help-body line — command names (`/foo`)
/// or shortcut keys (`Ctrl+C`) get the sky accent, the rest stays muted.
fn highlight_help_line(raw: &str, sky: Color, muted: Color) -> Line<'static> {
    // Line shape we expect: `  /command   description...` or
    // `  Ctrl+X    description...` or just `  • text...`. We split on the
    // first run of >=2 spaces.
    let trimmed = raw.trim_start();
    let leading = raw.len() - trimmed.len();
    // Find the first "double-space" gap that separates the keyword from
    // the description.
    if let Some(gap) = trimmed.find("  ") {
        let keyword = &trimmed[..gap];
        let rest = trimmed[gap..].trim_start();
        Line::from(vec![
            Span::raw(" ".repeat(leading)),
            Span::styled(
                keyword.to_string(),
                Style::default().fg(sky).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(rest.to_string(), Style::default().fg(muted)),
        ])
    } else {
        Line::from(Span::styled(raw.to_string(), Style::default().fg(muted)))
    }
}

/// Format a duration in seconds as a compact `1h2m` / `34m` / `12s` label.
fn format_duration_short(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs)
    }
}

/// Lines reserved at the bottom of the terminal for the live inline
/// viewport (status bar + input box + spinner room). Everything else flows
/// into the terminal's native scrollback.
/// Inline viewport height. Tight 5-row layout: 4 rows of input box +
/// 1 row of status bar. v0.6.50 dropped the stream-preview row that
/// used to duplicate the status bar's spinner/label.
pub const INLINE_VIEWPORT_LINES: u16 = 5;
/// Retained constant (= 0) so old call sites that used it as an offset
/// keep type-checking. The stream-preview pane was removed in v0.6.50;
/// re-introducing it means flipping this to `1` and adding the
/// `Constraint::Length(STREAM_PREVIEW_LINES)` back to the layout.
pub const STREAM_PREVIEW_LINES: u16 = 0;

/// Set up the terminal in **inline mode** — no alternate screen takeover.
///
/// The bottom `INLINE_VIEWPORT_LINES` rows of the terminal are reserved
/// for the TUI's live region (status bar + input box). Everything emitted
/// via `terminal.insert_before(...)` or plain `println!` lands in the
/// terminal's normal scrollback above that region. On exit the viewport
/// is consumed and the terminal returns to its prompt.
///
/// This is the Hermes / Claude-Code-style flow: chat history is the
/// terminal's own scrollback, not a ratatui List widget that fights it.
pub fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    // Opt into bracketed paste so multi-line pastes arrive as a single
    // `Event::Paste(text)` rather than a stream of `KeyCode::Char` + `\n` =
    // `KeyCode::Enter` events. Without this the first newline in a paste
    // auto-submits the prompt and the rest of the buffer becomes the
    // next turn(s). Terminals that don't understand the escape ignore
    // it and fall back to per-key delivery — same behavior as before.
    reinit_inline_terminal()
}

/// Build (or rebuild) the inline-viewport terminal AND re-arm bracketed paste.
///
/// Bracketed paste is a terminal *mode* (`ESC[?2004h`), not terminal state
/// ratatui tracks. Anything that resets the terminal clears it — most sharply
/// the RIS (`ESC c`) the resize handler emits — and `Terminal::with_options`
/// does not re-issue it. It was enabled exactly once, in `setup_terminal`,
/// against six `with_options` re-inits, so after a resize (or returning from
/// `$EDITOR`) pasted line breaks arrived as `Enter` again: a two-line paste
/// submitted its first line and left the rest. Re-arm here so every rebuild
/// carries the mode with it.
fn reinit_inline_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    let _ = execute!(io::stdout(), EnableBracketedPaste);
    Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(INLINE_VIEWPORT_LINES),
        },
    )
    .map_err(Into::into)
}

/// Suspend the TUI, hand control to `$EDITOR` (or `EDITOR`/`VISUAL`,
/// falling back to `nano`/`vi`/`notepad`), and copy the resulting
/// file contents back into `app.context.input_buffer` on success.
///
/// Best-effort: on any error (no editor on PATH, editor exited
/// non-zero, file IO failure) the original buffer is preserved and a
/// status-bar message surfaces the cause. Always restores raw mode
/// before returning so the caller can resume drawing.
fn run_external_editor(
    app: &mut TuiApp,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    use std::io::Read as _;
    use std::process::Command;

    // Resolve editor command: $EDITOR > $VISUAL > nano > vi > notepad.
    let editor_cmd = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                "notepad".to_string()
            } else if which_program("nano") {
                "nano".to_string()
            } else {
                "vi".to_string()
            }
        });

    // Write current buffer to a temp file; the editor edits in place.
    // Use a unique filename in the OS temp dir (no extra dep needed).
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_path = std::env::temp_dir().join(format!("rantaiclaw-prompt-{pid}-{nonce}.md"));
    std::fs::write(&tmp_path, &app.context.input_buffer)?;

    // Suspend the TUI: flush, leave alt-screen if needed, drop raw mode.
    let was_fullscreen = app.list_picker.is_some() || app.info_panel.is_some();
    if was_fullscreen {
        execute!(io::stdout(), LeaveAlternateScreen)?;
    }
    let _ = terminal.flush();
    disable_raw_mode()?;
    let _ = io::stdout().flush();

    // Run the editor. Inherit stdio so the user sees / interacts with it.
    let mut parts = editor_cmd.split_whitespace();
    let bin = parts.next().unwrap_or("vi");
    let args: Vec<&str> = parts.collect();
    let status = Command::new(bin).args(&args).arg(&tmp_path).status();

    // Always restore raw mode + alt-screen (if we were in it).
    enable_raw_mode()?;
    if was_fullscreen {
        execute!(io::stdout(), EnterAlternateScreen)?;
        terminal.clear()?;
    } else {
        // Inline mode: re-claim a fresh terminal so the inline viewport
        // is re-laid-out cleanly after the editor printed to the tty.
        *terminal = reinit_inline_terminal()?;
    }

    let result = match status {
        Ok(s) if s.success() => {
            let mut buf = String::new();
            std::fs::File::open(&tmp_path).and_then(|mut f| f.read_to_string(&mut buf))?;
            if buf.ends_with('\n') {
                buf.pop();
            }
            app.context.input_buffer = buf;
            app.context.cursor_to_end();
            app.context.exit_history_navigation();
            Ok(())
        }
        Ok(s) => {
            app.context.last_error = Some(format!(
                "editor exited with status {} — buffer unchanged",
                s.code().unwrap_or(-1)
            ));
            Ok(())
        }
        Err(e) => {
            app.context.last_error = Some(format!("editor '{bin}' failed to launch: {e}"));
            Ok(())
        }
    };
    // Best-effort cleanup; file is in $TMPDIR so leftovers are harmless.
    let _ = std::fs::remove_file(&tmp_path);
    result
}

/// Cheap PATH check so we can prefer `nano` over `vi` when available.
fn which_program(name: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

/// Swap the terminal into alt-screen / fullscreen mode. Used while a
/// list picker is open so it can claim the entire terminal height
/// instead of fighting for space inside the 6-row inline viewport.
/// The original scrollback is preserved by the terminal emulator and
/// restored automatically on `swap_to_inline`.
pub fn swap_to_fullscreen(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let _ = terminal.flush();
    execute!(io::stdout(), EnterAlternateScreen)?;
    *terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    Ok(())
}

/// Swap the terminal back to the inline 6-row viewport after leaving the
/// alt-screen picker. The inline viewport is re-created fresh; existing
/// terminal scrollback (committed via `insert_before` before the picker
/// opened) is automatically restored by the terminal when alt-screen is
/// left.
pub fn swap_to_inline(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let _ = terminal.flush();
    execute!(io::stdout(), LeaveAlternateScreen)?;
    *terminal = reinit_inline_terminal()?;
    Ok(())
}

/// Restore the terminal to its original state. Inline mode means no
/// alt-screen to leave; we just flush the viewport (so the cursor lands
/// below it cleanly) and disable raw mode.
pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    // Move cursor to a fresh line below the viewport so the user's shell
    // prompt doesn't print on top of our last frame.
    terminal.clear()?;
    let _ = terminal.show_cursor();
    // Pair with the EnableBracketedPaste in setup_terminal so the user's
    // shell after we exit doesn't inherit the bracketed-paste mode.
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    disable_raw_mode()?;
    let _ = io::stdout().flush();
    println!();
    Ok(())
}

/// Entry point for the TUI: guards TTY, builds the `Agent` + actor bridge,
/// runs the event loop, then shuts the actor down cleanly.
///
/// The TUI talks to the `Agent` exclusively through an mpsc bridge:
///   * `req_tx`/`req_rx` — TUI -> actor (`TurnRequest::Submit`/`Cancel`)
///   * `events_tx`/`events_rx` — actor -> TUI (`AgentEvent` stream)
///
/// Config is loaded here (rather than passed in) to avoid the binary/library
/// `Config` duplication that results from the bin+lib sharing `src/config/`.
/// `Agent::from_config` lives in the library crate, so it must receive the
/// library-side `Config`, which we obtain via the library's own loader.
///
/// On exit we drop the `TuiApp` (which releases `req_tx`), giving the actor
/// `None` from `req_rx.recv()` so it can finish its current turn and return.
/// A bounded timeout avoids hanging shutdown if the actor is stuck.
/// Count how many transport channels have a non-empty configuration in
/// `config.channels_config`. Used by the TUI auto-start path to decide
/// whether to spawn `start_channels` and by `/channels` + `/platforms`
/// to render the active set. The CLI surface is implicit (always on)
/// and is not counted here.
pub(crate) fn count_configured_channels(c: &crate::config::Config) -> usize {
    let mut n = 0;
    let cc = &c.channels_config;
    if cc.telegram.is_some() {
        n += 1;
    }
    if cc.discord.is_some() {
        n += 1;
    }
    if cc.slack.is_some() {
        n += 1;
    }
    if cc.mattermost.is_some() {
        n += 1;
    }
    if cc.webhook.is_some() {
        n += 1;
    }
    if cc.imessage.is_some() {
        n += 1;
    }
    if cc.signal.is_some() {
        n += 1;
    }
    if cc.whatsapp.is_some() {
        n += 1;
    }
    if cc.linq.is_some() {
        n += 1;
    }
    if cc.nextcloud_talk.is_some() {
        n += 1;
    }
    if cc.email.is_some() {
        n += 1;
    }
    if cc.irc.is_some() {
        n += 1;
    }
    if cc.dingtalk.is_some() {
        n += 1;
    }
    #[cfg(feature = "channel-matrix")]
    {
        if cc.matrix.is_some() {
            n += 1;
        }
    }
    #[cfg(feature = "channel-lark")]
    {
        if cc.lark.is_some() {
            n += 1;
        }
    }
    n
}

/// Per-channel state for the `/channels` and `/platforms` commands.
/// `(name, configured, transport-hint)`. `configured=true` means the
/// channel has a config block in `config.toml`; whether it's actually
/// polling depends on whether `channels_autostart_count > 0` was true
/// at TUI startup.
/// Per-channel "is this actually configured" — the credential is present and
/// non-empty, not merely that the `[channels_config.<x>]` section exists.
///
/// The old check was `section.is_some()`, so a block with an empty `bot_token`
/// (a hand-edit, or an aborted `/setup`) rendered "✓ configured". Each channel
/// carries its own notion of a credential, so this checks the right field per
/// channel rather than one uniform rule:
///
/// - token-bearing channels: the required credential string is non-blank;
/// - iMessage / Webhook: presence *is* configuration — iMessage drives the
///   local Messages app and has no credential, Webhook is an inbound receiver
///   keyed by a port;
/// - WhatsApp: cloud (`access_token`) OR web (`session_path`), mirroring
///   `doctor::checks::channels::inspect_channels`;
/// - DingTalk / Email: left as presence-only. Their required credential is not
///   determinable from the struct alone (DingTalk has four credential-ish
///   fields; Email lives in another module with IMAP+SMTP auth), and guessing
///   would risk the same false report this fixes. Tightening them needs their
///   construction code read — tracked as follow-up.
pub(crate) fn channel_status_summary(c: &crate::config::Config) -> Vec<(&'static str, bool)> {
    let cc = &c.channels_config;
    let non_blank = |s: &str| !s.trim().is_empty();

    #[allow(unused_mut)]
    let mut rows: Vec<(&'static str, bool)> = vec![
        (
            "Telegram",
            cc.telegram
                .as_ref()
                .is_some_and(|t| non_blank(&t.bot_token)),
        ),
        (
            "Discord",
            cc.discord.as_ref().is_some_and(|d| non_blank(&d.bot_token)),
        ),
        (
            "Slack",
            cc.slack.as_ref().is_some_and(|s| non_blank(&s.bot_token)),
        ),
        (
            "WhatsApp",
            cc.whatsapp.as_ref().is_some_and(|w| {
                w.access_token.as_deref().is_some_and(non_blank)
                    || w.session_path.as_deref().is_some_and(non_blank)
            }),
        ),
        (
            "Mattermost",
            cc.mattermost
                .as_ref()
                .is_some_and(|m| non_blank(&m.url) && non_blank(&m.bot_token)),
        ),
        (
            "Signal",
            cc.signal
                .as_ref()
                .is_some_and(|s| non_blank(&s.http_url) && non_blank(&s.account)),
        ),
        // Email / DingTalk: presence-only, see fn doc.
        ("Email", cc.email.is_some()),
        (
            "IRC",
            cc.irc
                .as_ref()
                .is_some_and(|i| non_blank(&i.server) && non_blank(&i.nickname)),
        ),
        ("DingTalk", cc.dingtalk.is_some()),
        // Webhook: an inbound receiver; presence is configuration.
        ("Webhook", cc.webhook.is_some()),
        (
            "Linq",
            cc.linq.as_ref().is_some_and(|l| non_blank(&l.api_token)),
        ),
        (
            "Nextcloud Talk",
            cc.nextcloud_talk
                .as_ref()
                .is_some_and(|n| non_blank(&n.base_url) && non_blank(&n.app_token)),
        ),
        // iMessage: local Messages app, no credential; presence is configuration.
        ("iMessage", cc.imessage.is_some()),
    ];
    #[cfg(feature = "channel-matrix")]
    {
        rows.push((
            "Matrix",
            cc.matrix
                .as_ref()
                .is_some_and(|m| non_blank(&m.homeserver) && non_blank(&m.access_token)),
        ));
    }
    #[cfg(feature = "channel-lark")]
    {
        rows.push((
            "Lark / Feishu",
            cc.lark
                .as_ref()
                .is_some_and(|l| non_blank(&l.app_id) && non_blank(&l.app_secret)),
        ));
    }
    rows
}

pub async fn run_tui(tui_config: TuiConfig) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!("TUI requires an interactive terminal (stdin is not a TTY)");
    }

    // Install a file-writing tracing subscriber BEFORE we touch any code
    // that might emit logs. Without this, tracing falls through to the
    // default-stderr writer, and `tracing::warn!` calls from the agent's
    // provider/retry path bleed straight into the TUI's alt-screen frame.
    // Best-effort: failure to set it up just falls back to stderr (which
    // is rare in practice and worth knowing about).
    install_tui_tracing();

    let mut app_config = crate::config::Config::load_or_init().await?;
    app_config.apply_env_overrides();

    // If TuiConfig still has its compile-time default (no /model flag was
    // passed), surface the active config's provider:model so the status
    // bar reflects what the agent will actually use.
    let mut tui_config = tui_config;
    if tui_config.model == TuiConfig::default().model {
        let provider = app_config.default_provider.clone().unwrap_or_default();
        let model = app_config.default_model.clone().unwrap_or_default();
        if !provider.is_empty() && !model.is_empty() {
            tui_config.model = format!("{}:{}", provider, model);
        } else if !model.is_empty() {
            tui_config.model = model;
        }
    }

    let mut agent = Agent::from_config(&app_config).await?;

    // `/resume`: re-feed the resumed session's prior turns so the model
    // actually remembers the earlier conversation (not just the scrollback).
    if let Some(resume_id) = tui_config.resume_session.as_deref() {
        match crate::sessions::cli::open_store().and_then(|s| s.get_messages(resume_id)) {
            Ok(msgs) => {
                let prior = crate::sessions::messages_to_turns(&msgs);
                if !prior.is_empty() {
                    if let Err(e) = agent.restore_history(&prior) {
                        tracing::warn!("failed to restore resumed history: {e}");
                    }
                }
            }
            Err(e) => tracing::warn!("could not load resumed session {resume_id}: {e}"),
        }
    }

    let profile =
        crate::profile::ProfileManager::active().unwrap_or_else(|_| crate::profile::Profile {
            name: "default".to_string(),
            root: crate::profile::paths::profile_dir("default"),
        });

    // Channel capacities are intentionally small on the request side (user
    // input is human-paced) and larger on the event side (streaming chunks
    // burst quickly per turn).
    let (req_tx, req_rx) = mpsc::channel::<TurnRequest>(16);
    let (events_tx, events_rx): (AgentEventSender, mpsc::Receiver<AgentEvent>) = mpsc::channel(128);

    let security_handle = agent.security();
    let memory_handle = agent.memory_handle();
    let mcp_tools_by_server = agent.mcp_tools_by_server();
    let actor = TuiAgentActor::new(agent, req_rx, events_tx);
    let actor_handle = tokio::spawn(actor.run());

    // Compute the list of providers visible to /model. Starts with the
    // primary `default_provider`, then any unique providers used by
    // `model_routes`. This is the "enabled" set surfaced in the picker.
    let mut available_providers: Vec<String> = Vec::new();
    if let Some(p) = app_config.default_provider.clone() {
        available_providers.push(p);
    }
    for route in &app_config.model_routes {
        if !available_providers.iter().any(|p| p == &route.provider) {
            available_providers.push(route.provider.clone());
        }
    }

    let mut app = TuiApp::new(
        &tui_config,
        app_config.clone(),
        profile.clone(),
        req_tx,
        events_rx,
    )?;
    app.context.available_providers = available_providers;
    // Subscribe to the pending-approvals broadcast before stashing the
    // security handle on the context, so that the moment the shell tool
    // suspends a turn waiting for /allow|/deny, the TUI sees the
    // notification and surfaces a system message.
    if let Some(security) = security_handle.as_ref() {
        if let Some(pending) = security.pending() {
            app.pending_approvals_rx = Some(pending.subscribe());
        }
    }
    app.context.security = security_handle;
    app.context.memory = Some(memory_handle);
    app.context.autonomy_preset =
        crate::approval::policy_writer::read_active_preset(&profile.policy_dir());

    // Surface MCP server config + discovered tools so `/mcp` can
    // render a useful diff between "configured" and "actually live".
    app.context.mcp_servers_configured = app_config.mcp_servers.keys().cloned().collect();
    app.context.mcp_tools_by_server = mcp_tools_by_server;

    // v0.6.51 deprecation: the curated `filesystem` MCP was dropped
    // because it duplicates the built-in shell / file_read / file_write
    // tools at the cost of ~80MB of node + 2 wasted iterations per fs
    // op. We don't auto-remove the user's config entry (their data),
    // just surface a one-shot warning so they know they can clean it
    // up. Detection is loose — anything with `filesystem` in the slug
    // and an npx `@modelcontextprotocol/server-filesystem` command.
    if let Some((slug, entry)) = app_config.mcp_servers.iter().find(|(s, e)| {
        s.contains("filesystem")
            && e.command == "npx"
            && e.args
                .iter()
                .any(|a| a.contains("@modelcontextprotocol/server-filesystem"))
    }) {
        let _ = app.context.append_system_message(&format!(
            "⚠ MCP server `{slug}` (`@modelcontextprotocol/server-filesystem`) is deprecated \
             in rantaiclaw. The built-in `shell`/`file_read`/`file_write` tools cover the same \
             surface without the npx overhead. Remove the `[mcp_servers.{slug}]` block from \
             config.toml to free up ~80MB of node + reduce wasted tool iterations."
        ));
        let _ = entry; // suppress unused warning when args check skipped
    }

    if let Some(topic) = tui_config.setup_provisioner.take() {
        // `rantaiclaw setup` (no topic) and `rantaiclaw setup full` both
        // boot the first-run wizard — the canonical "set everything up"
        // entry point. Named topics route to the overlay for that one
        // provisioner; unknown names surface the existing error.
        // Resolve a category name only when nothing else claims the topic.
        // `rantaiclaw setup channels` is documented in
        // docs/reference/commands.md and printed by the post-setup banner, yet
        // only its `--non-interactive` form worked: the headless path falls
        // back to `onboard::wizard::run_setup`, which knows section names,
        // while this interactive path had no fallback at all. Same command,
        // opposite outcome depending on a flag.
        let category = if crate::onboard::provision::provisioner_for(&topic).is_none() {
            crate::tui::commands::setup::category_from_arg(&topic)
        } else {
            None
        };

        if topic.is_empty() || topic.eq_ignore_ascii_case("full") {
            app.first_run_wizard = Some(crate::tui::FirstRunWizard::new(profile.clone()));
        } else if let Some(cat) = category {
            app.open_category_sub_picker(crate::tui::commands::setup::category_key(cat));
        } else if let Err(e) = app.open_setup_overlay(topic) {
            let msg = format!("Failed to open setup: {}", e);
            let _ = app.context.append_system_message(&msg);
            app.scrollback_queue.push(("system".to_string(), msg));
        }
    } else if app_config.api_key.is_none() && app_config.default_provider.is_none() {
        app.first_run_wizard = Some(crate::tui::FirstRunWizard::new(profile.clone()));
    }

    // Console login gate — when enabled, must be passed before the app is usable.
    // It renders over everything and intercepts all input at the top of the key
    // handler, so it takes precedence over the first-run wizard.
    if app_config.gateway.login.password_hash.is_some() {
        app.login_gate = Some(crate::tui::LoginGateState::new(
            app_config.gateway.login.username.clone(),
        ));
    }

    // Idempotent first-run skill seeding: if the workspace skills dir
    // is empty, drop the 5-skill starter pack (web-search, summarizer,
    // research-assistant, scheduler-reminders, meeting-notes) so the
    // /skills picker has real content out of the box. Mirrors Hermes'
    // bundled-skills-on-install UX. Best-effort — failure is logged but
    // doesn't block startup.
    let _ = crate::skills::bundled::install_starter_pack(&profile);
    // Load skills from the active workspace so /skills can browse them.
    // load_skills_with_config falls back to an empty vec on any error
    // (missing dir, malformed manifest, etc.) — never blocks startup.
    app.refresh_available_skills();
    let workspace_skills = app_config.workspace_dir.join("skills");
    match crate::skills::watcher::SkillsWatcher::watch(&profile.skills_dir(), &workspace_skills) {
        Ok(watcher) => {
            app.skills_watcher = Some(watcher);
        }
        Err(e) => {
            tracing::warn!("skill watcher disabled: {e:#}");
        }
    }

    // Config.toml file watcher — direct edits to the active profile's
    // config trigger a reload, mirroring the wizard-close path. This
    // is what makes `[mcp_servers.foo]` added by hand take effect
    // without restarting rantaiclaw.
    match crate::config::watcher::ConfigWatcher::watch(&app_config.config_path) {
        Ok(watcher) => {
            app.config_watcher = Some(watcher);
        }
        Err(e) => {
            tracing::warn!("config watcher disabled: {e:#}");
        }
    }

    // Auto-start configured channel listeners alongside the TUI. Before
    // v0.6.4 the TUI was a single local-chat surface — Telegram / Discord /
    // Slack / etc. configured via the wizard would never receive messages
    // because nothing was polling them; users had to run a separate
    // `rantaiclaw daemon`. Tester reports surfaced this as "bot doesn't
    // reply outside the TUI." Now bare `rantaiclaw` is the canonical
    // multi-channel runtime: TUI owns the local terminal, channels run as
    // a background task in the same process.
    //
    // Failure-mode discipline: never block the TUI on channel startup.
    // If start_channels errors (bad token, network, missing creds), the
    // user can still chat locally; the failure is logged + surfaced via
    // /channels.
    let configured_channels = count_configured_channels(&app_config);
    app.context.channels_summary = channel_status_summary(&app_config)
        .into_iter()
        .map(|(name, configured)| (name.to_string(), configured))
        .collect();
    if configured_channels > 0 {
        // Spawn the channel runtime as a cancellable supervisor (stored on
        // `app`) rather than a fire-and-forget task, so a mid-session
        // `/setup <channel>` or skill add can restart it in place via
        // `restart_channels`. `app.config` is `app_config.clone()` (see the
        // `TuiApp::new` call above), so this uses the same decrypted config.
        app.restart_channels();
    }

    let mut terminal = setup_terminal()?;

    // Splash banner — committed once to the terminal's scrollback before
    // the inline viewport takes over. Becomes permanent history above the
    // status bar / input region.
    let _ = TuiApp::commit_splash_to_scrollback(&mut terminal, &app.context);

    let result = run_loop(&mut app, &mut terminal).await;

    // Always restore terminal, even on error.
    let restore_result = restore_terminal(&mut terminal);

    // Drop the app so the actor's req_rx sees all senders gone and exits.
    drop(app);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), actor_handle).await;

    // Surface the loop error first, then the restore error.
    result?;
    restore_result?;

    Ok(())
}

/// Inner event loop separated from terminal lifecycle for easier testing.
async fn run_loop(
    app: &mut TuiApp,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
) -> Result<()> {
    // Temporary fullscreen Terminal instantiated only while a list
    // picker is open. The inline `terminal` (parameter) is NEVER
    // dropped/recreated during alt-screen swaps — that's what caused
    // the scrollback whitespace bug. Instead we send the alt-screen
    // escape and use a separate Fullscreen Terminal sharing stdout for
    // the duration. Returning to inline just drops `alt` and emits
    // LeaveAlternateScreen — the original screen state (including the
    // inline viewport rows) is restored by the terminal emulator.
    let mut alt: Option<Terminal<CrosstermBackend<Stdout>>> = None;
    loop {
        // Drain any buffered agent events before rendering so the frame
        // reflects the latest streaming state.
        app.drain_events();

        // Alt-screen entry/exit covers every full-screen surface — the
        // console login gate, list picker, info panel, slash-autocomplete
        // dropdown, setup overlay, and first-run wizard. Each needs the
        // whole terminal instead of the ~5-row inline viewport. Edge-
        // triggered via option presence so we don't churn buffers on
        // every keystroke.
        let want_alt = app.login_gate.is_some()
            || app.list_picker.is_some()
            || app.info_panel.is_some()
            || app.autocomplete.is_visible()
            || app.setup_overlay.is_some()
            || app.first_run_wizard.is_some();
        let in_alt = alt.is_some();
        if want_alt && !in_alt {
            execute!(io::stdout(), EnterAlternateScreen)?;
            let mut t = Terminal::new(CrosstermBackend::new(io::stdout()))?;
            t.clear()?;
            alt = Some(t);
        } else if !want_alt && in_alt {
            // Drop the temp fullscreen terminal first so its final flush
            // happens INSIDE alt-screen, then leave alt-screen, then
            // rebuild the inline terminal so its internal viewport-row
            // tracking is reset.
            //
            // Pre-fix this path just called `terminal.clear()` and
            // assumed the original screen state was restored cleanly by
            // `LeaveAlternateScreen`. In practice the inline viewport's
            // row anchor drifted across the swap — the next render drew
            // the viewport at a new row while the previous frame stayed
            // pinned in scrollback, producing duplicate input boxes and
            // status bars after closing `/skills`, `/sessions`, etc.
            //
            // Wiping the visible screen with `\x1b[2J\x1b[H` and then
            // recreating the inline Terminal forces ratatui to claim
            // fresh viewport rows at the bottom of the now-empty screen.
            // Scrollback contents above are untouched (no `\x1b[3J`).
            drop(alt.take());
            execute!(io::stdout(), LeaveAlternateScreen)?;
            let _ = terminal.flush();
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[2J\x1b[H");
            let _ = out.flush();
            *terminal = reinit_inline_terminal()?;
        }

        // /new and /clear request a full screen+scrollback wipe so
        // the next session starts on a clean terminal — same intent
        // as running `clear` at the shell. ESC[3J clears the xterm
        // scrollback buffer; ESC[2J clears the visible screen; ESC[H
        // homes the cursor. Then re-claim a fresh inline viewport and
        // re-print the splash banner so the user lands on the same
        // welcome screen as a cold launch (`./rantaiclaw`).
        if app.clear_terminal_request && alt.is_none() {
            app.clear_terminal_request = false;
            let _ = terminal.flush();
            let mut out = io::stdout();
            let _ = out.write_all(b"\x1b[3J\x1b[2J\x1b[H");
            let _ = out.flush();
            *terminal = reinit_inline_terminal()?;
            let _ = TuiApp::commit_splash_to_scrollback(terminal, &app.context);
        }

        // Inline-only: stream completed lines into scrollback and flush
        // any queued message commits ABOVE the viewport. Skipped while
        // the picker has us in alt-screen — those commits would write
        // into the alt buffer and be lost when we return.
        if alt.is_none() {
            app.flush_stream_to_scrollback(terminal)?;
            let pending: Vec<(String, String)> = std::mem::take(&mut app.scrollback_queue);
            for (role, content) in pending {
                TuiApp::commit_message_to_scrollback(terminal, &role, &content)?;
            }
        }

        if let Some(ref mut alt_term) = alt {
            // Render priority: the console login gate owns the screen
            // above everything else until the password verifies.
            if app.login_gate.is_some() {
                alt_term.draw(|frame| {
                    let area = frame.area();
                    if let Some(gate) = app.login_gate.as_ref() {
                        gate.render_fullscreen(frame, area);
                    }
                })?;
            }
            // Then setup_overlay. During the first-run wizard's
            // RunningProvisioner phase BOTH wizard and overlay are active —
            // the wizard intentionally renders nothing in that phase and
            // delegates the screen to the overlay. If the wizard won the
            // priority race, the screen would go black.
            else if app.setup_overlay.is_some() {
                alt_term.draw(|frame| {
                    let area = frame.area();
                    if let Some(o) = app.setup_overlay.as_mut() {
                        o.render(frame, area);
                    }
                })?;
            } else if app.first_run_wizard.is_some() {
                alt_term.draw(|frame| {
                    let area = frame.area();
                    if let Some(w) = app.first_run_wizard.as_mut() {
                        w.render_fullscreen(frame, area);
                    }
                })?;
            } else if app.list_picker.is_some() {
                app.render_fullscreen_picker(alt_term)?;
            } else if app.info_panel.is_some() {
                app.render_fullscreen_info_panel(alt_term)?;
            } else {
                app.render_fullscreen_autocomplete(alt_term)?;
            }
        } else {
            app.render(terminal)?;
        }

        // Tighten the poll interval during streaming so the live preview
        // updates fast enough to feel like word-by-word streaming. When
        // idle, poll less aggressively to keep CPU near zero. Also tighten
        // while a ClawHub install is in flight so the spinner animation
        // ticks smoothly (~12 fps).
        let poll_ms = if matches!(app.state, AppState::Streaming { .. }) {
            16
        } else if app.clawhub_install_in_progress.is_some() {
            80
        } else {
            100
        };
        if event::poll(std::time::Duration::from_millis(poll_ms))? {
            let ev = event::read()?;
            // Inline mode pins a 6-row viewport to the bottom of the
            // terminal. When the terminal is resized, ratatui repaints
            // the viewport at the new bottom row, but the previous
            // viewport's rows remain in the terminal buffer above as
            // ghost copies. Drain any coalesced Resize events, then
            // wipe the screen+scrollback and replay the splash +
            // message history so the terminal looks like a fresh
            // launch at the new size.
            //
            // The clear sequence layers three escapes for portability:
            //   - `\x1b[3J`  xterm-extension scrollback clear (most
            //     modern terminals: tmux, alacritty, kitty, wezterm,
            //     Windows Terminal, iTerm2, gnome-terminal, …).
            //   - `\x1b[2J`  clear visible region.
            //   - `\x1bc`    RIS / Full Reset — fallback for terminals
            //     that ignore `\x1b[3J`, notably the VS Code built-in
            //     terminal. RIS does reset other state (cursor style,
            //     character set), but the immediately-following
            //     `Terminal::with_options` + ratatui rendering
            //     restores everything we care about.
            // While in alt-screen the picker handles its own sizing;
            // just trigger a repaint there.
            if matches!(ev, Event::Resize(_, _)) {
                while event::poll(std::time::Duration::from_millis(0))? {
                    let next = event::read()?;
                    if !matches!(next, Event::Resize(_, _)) {
                        match app.handle_event(next).await? {
                            EventResult::Quit => break,
                            EventResult::Continue => {}
                        }
                        break;
                    }
                }
                if alt.is_none() {
                    let _ = terminal.flush();
                    let mut out = io::stdout();
                    let _ = out.write_all(b"\x1bc\x1b[3J\x1b[2J\x1b[H");
                    let _ = out.flush();
                    *terminal = reinit_inline_terminal()?;
                    let _ = TuiApp::commit_splash_to_scrollback(terminal, &app.context);
                    let messages = app.context.messages.clone();
                    for msg in messages {
                        let _ =
                            TuiApp::commit_message_to_scrollback(terminal, &msg.role, &msg.content);
                    }
                } else {
                    let _ = terminal.clear();
                }
            } else {
                match app.handle_event(ev).await? {
                    EventResult::Quit => break,
                    EventResult::Continue => {}
                }
            }
        }

        // Handle deferred editor request raised by Ctrl+G. Done after
        // event handling so the buffer reflects the user's latest
        // keystrokes, and before the next render so the new contents
        // appear on the next frame.
        if app.editor_request {
            app.editor_request = false;
            if let Err(e) = run_external_editor(app, terminal) {
                app.context.last_error = Some(format!("editor flow error: {e}"));
            }
        }

        if matches!(app.state, AppState::Quitting) {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod error_format_tests {
    use super::format_agent_error;

    #[test]
    fn api_key_missing_gets_friendly_chat_body() {
        let raw = "All providers/models failed. Attempts:\nprovider=openrouter model=anthropic/claude-sonnet-4.6 attempt 1/3: non_retryable; error=OpenRouter API key not set. Run `rantaiclaw onboard` or set OPENROUTER_API_KEY env var.";
        let (chat, status) = format_agent_error(raw, "openrouter");
        assert!(chat.contains("Missing API key"));
        assert!(chat.contains("OPENROUTER_API_KEY"));
        assert!(!chat.contains("attempt 1/3"));
        assert!(status.contains("missing API key"));
    }

    #[test]
    fn rate_limit_gets_short_message() {
        let raw = "All providers/models failed. Attempts:\nprovider=openrouter model=x attempt 1/3: rate-limited; HTTP 429";
        let (chat, status) = format_agent_error(raw, "openrouter");
        assert!(chat.contains("Rate-limited"));
        assert!(status.contains("rate limit"));
    }

    #[test]
    fn unknown_error_compacts_attempts_tail() {
        let raw = "All providers/models failed. Attempts:\nprovider=p1 model=m attempt 1/3: foo\nprovider=p2 model=m attempt 1/3: bar";
        let (chat, _status) = format_agent_error(raw, "openrouter");
        // Compacted to first attempt plus +N more rather than full transcript.
        assert!(chat.contains("(+1 more attempts)"), "got {chat:?}");
    }

    #[test]
    fn non_provider_error_passes_through_with_prefix() {
        let (chat, _) = format_agent_error("something else broke", "openrouter");
        assert!(chat.starts_with("✗ something else broke"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_telegram(token: &str) -> crate::config::Config {
        let mut c = crate::config::Config::default();
        c.channels_config.telegram = Some(
            serde_json::from_value(serde_json::json!({
                "bot_token": token,
                "allowed_users": [],
            }))
            .unwrap(),
        );
        c
    }

    fn is_configured(rows: &[(&'static str, bool)], name: &str) -> bool {
        rows.iter()
            .find(|(n, _)| *n == name)
            .map(|(_, c)| *c)
            .unwrap()
    }

    /// The bug: a `[channels_config.telegram]` block with an empty bot_token —
    /// a hand-edit or aborted /setup — used to render "✓ configured" because
    /// the check was `.is_some()`.
    #[test]
    fn empty_bot_token_reads_as_not_configured() {
        let rows = channel_status_summary(&cfg_with_telegram(""));
        assert!(!is_configured(&rows, "Telegram"));
        let rows = channel_status_summary(&cfg_with_telegram("   "));
        assert!(
            !is_configured(&rows, "Telegram"),
            "whitespace is not a token"
        );
    }

    #[test]
    fn a_real_bot_token_reads_as_configured() {
        let rows = channel_status_summary(&cfg_with_telegram("123:XYZ"));
        assert!(is_configured(&rows, "Telegram"));
    }

    #[test]
    fn an_absent_channel_reads_as_not_configured() {
        let rows = channel_status_summary(&crate::config::Config::default());
        assert!(!is_configured(&rows, "Telegram"));
        assert!(!is_configured(&rows, "Discord"));
    }

    /// iMessage has no credential (it drives the local Messages app), so its
    /// presence IS its configuration — a credential check would wrongly flip a
    /// working iMessage setup to "not configured".
    #[test]
    fn imessage_is_configured_by_presence_alone() {
        let mut c = crate::config::Config::default();
        c.channels_config.imessage =
            Some(serde_json::from_value(serde_json::json!({ "allowed_contacts": [] })).unwrap());
        let rows = channel_status_summary(&c);
        assert!(is_configured(&rows, "iMessage"));
    }
    use crate::sessions::SessionStore;
    use crate::tui::context::TuiContext;

    fn make_app_from_store(store: SessionStore, model: &str) -> TuiApp {
        // Tests that hit this helper do not exercise the bridge; the request
        // receiver and events sender are held locally so the TUI-side ends
        // stay valid for the duration of the test.
        let (req_tx, _req_rx) = tokio::sync::mpsc::channel(4);
        let (_events_tx, events_rx) = tokio::sync::mpsc::channel(32);
        let ctx = TuiContext::new(store, model, None, req_tx, events_rx).expect("context");
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config: crate::config::Config::default(),
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: crate::profile::paths::profile_dir("default"),
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_save_complete_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            clawhub_install_last_query: String::new(),
            clawhub_install_search_version: 0,
            clawhub_install_results_rx: None,
            clawhub_install_results_tx: None,
            clawhub_install_in_progress: None,
            clawhub_install_completion_rx: None,
            clawhub_install_completion_tx: None,
            skill_deps_install_in_progress: None,
            skill_deps_install_completion_rx: None,
            skill_deps_install_completion_tx: None,
            skill_deps_install_finished_at: None,
            skills_watcher: None,
            config_watcher: None,
            channel_supervisor: None,
            wizard_install_in_progress: false,
            wizard_installed_slugs: Vec::new(),
            info_panel: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
            login_gate: None,
            pending_approvals_rx: None,
            shell_blocks_this_turn: 0,
            autonomy_hint_shown_this_turn: false,
            pending_approval: None,
        }
    }

    #[test]
    fn app_defers_session_until_first_message() {
        let store = SessionStore::in_memory().expect("store");
        let app = make_app_from_store(store, "test-model");

        // Launching alone must not create a session — only the first message does.
        assert!(app.context.session_id.is_none());
        assert!(matches!(app.state, AppState::Ready));
        assert!(app.context.messages.is_empty());
    }

    #[tokio::test]
    async fn app_handles_quit_command() {
        let store = SessionStore::in_memory().expect("store");
        let mut app = make_app_from_store(store, "test-model");

        app.context.input_buffer = "/quit".to_string();
        app.submit_input().await.unwrap();

        assert!(matches!(app.state, AppState::Quitting));
    }

    #[tokio::test]
    async fn app_handles_new_command() {
        let store = SessionStore::in_memory().expect("store");
        let mut app = make_app_from_store(store, "test-model");

        // A non-command submit now dispatches via the bridge and transitions
        // the app to Streaming (no real actor in this test — the request
        // simply sits in the channel). The user message is still appended
        // locally, which is what this test originally covered.
        app.context.input_buffer = "hello".to_string();
        app.submit_input().await.unwrap();
        assert!(!app.context.messages.is_empty());
        // The first message lazily bound a session; capture it to prove /new flips it.
        let first_session_id = app.context.session_id.clone();
        assert!(first_session_id.is_some());

        app.context.input_buffer = "/new".to_string();
        app.submit_input().await.unwrap();

        // /new clears the chat, then appends a system "Started new session"
        // confirmation line — so messages == 1, not 0. The session id flips.
        assert_eq!(app.context.messages.len(), 1);
        assert_eq!(app.context.messages[0].role, "system");
        assert_ne!(app.context.session_id, first_session_id);
    }
}

#[cfg(test)]
mod submit_tests {
    use super::*;
    use crate::tui::async_bridge::TurnRequest;
    use crate::tui::context::TuiContext;

    fn make_app_with_context(ctx: TuiContext) -> TuiApp {
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config: crate::config::Config::default(),
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: crate::profile::paths::profile_dir("default"),
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_save_complete_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            clawhub_install_last_query: String::new(),
            clawhub_install_search_version: 0,
            clawhub_install_results_rx: None,
            clawhub_install_results_tx: None,
            clawhub_install_in_progress: None,
            clawhub_install_completion_rx: None,
            clawhub_install_completion_tx: None,
            skill_deps_install_in_progress: None,
            skill_deps_install_completion_rx: None,
            skill_deps_install_completion_tx: None,
            skill_deps_install_finished_at: None,
            skills_watcher: None,
            config_watcher: None,
            channel_supervisor: None,
            wizard_install_in_progress: false,
            wizard_installed_slugs: Vec::new(),
            info_panel: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
            login_gate: None,
            pending_approvals_rx: None,
            shell_blocks_this_turn: 0,
            autonomy_hint_shown_this_turn: false,
            pending_approval: None,
        }
    }

    #[tokio::test]
    async fn submit_input_ready_state_sends_request_and_transitions_to_streaming() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Ready;
        app.context.input_buffer = "hello".into();

        app.submit_input().await.unwrap();

        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert_eq!(app.context.input_buffer, "");
        let req = req_rx.recv().await.expect("request should have been sent");
        match req {
            TurnRequest::Submit(text) => assert_eq!(text, "hello"),
            TurnRequest::Cancel => panic!("expected Submit, got Cancel"),
            TurnRequest::Reload(_) => panic!("expected Submit, got Reload"),
            TurnRequest::Compact { .. } => panic!("expected Submit, got Compact"),
        }
    }

    #[tokio::test]
    async fn submit_input_streaming_state_queues_and_increments_counter() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };
        app.context.input_buffer = "queued".into();

        app.submit_input().await.unwrap();

        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert_eq!(app.context.queued_turns, 1);
        let req = req_rx.recv().await.expect("request should have been sent");
        match req {
            TurnRequest::Submit(text) => assert_eq!(text, "queued"),
            TurnRequest::Cancel => panic!("expected Submit, got Cancel"),
            TurnRequest::Reload(_) => panic!("expected Submit, got Reload"),
            TurnRequest::Compact { .. } => panic!("expected Submit, got Compact"),
        }
    }

    #[tokio::test]
    async fn submit_input_empty_buffer_is_noop() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Ready;
        app.context.input_buffer = "   ".into(); // whitespace only

        app.submit_input().await.unwrap();

        assert!(matches!(app.state, AppState::Ready));
        assert!(
            req_rx.try_recv().is_err(),
            "no request should have been sent for whitespace-only buffer"
        );
    }
}

#[cfg(test)]
mod ctrl_c_tests {
    use super::*;
    use crate::tui::async_bridge::TurnRequest;
    use crate::tui::context::TuiContext;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn make_app_with_context(ctx: TuiContext) -> TuiApp {
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config: crate::config::Config::default(),
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: crate::profile::paths::profile_dir("default"),
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_save_complete_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            clawhub_install_last_query: String::new(),
            clawhub_install_search_version: 0,
            clawhub_install_results_rx: None,
            clawhub_install_results_tx: None,
            clawhub_install_in_progress: None,
            clawhub_install_completion_rx: None,
            clawhub_install_completion_tx: None,
            skill_deps_install_in_progress: None,
            skill_deps_install_completion_rx: None,
            skill_deps_install_completion_tx: None,
            skill_deps_install_finished_at: None,
            skills_watcher: None,
            config_watcher: None,
            channel_supervisor: None,
            wizard_install_in_progress: false,
            wizard_installed_slugs: Vec::new(),
            info_panel: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
            login_gate: None,
            pending_approvals_rx: None,
            shell_blocks_this_turn: 0,
            autonomy_hint_shown_this_turn: false,
            pending_approval: None,
        }
    }

    /// The `KeyCode::Char(c)` insert arm gated only on "no overlay / no
    /// wizard" and never looked at `key.modifiers`, so every Ctrl chord the
    /// app does not explicitly handle fell through and typed its own letter.
    /// Reproduced live: typing `hello` then Ctrl+A/E/W/K/U left `helloaewku`.
    #[tokio::test]
    async fn unhandled_ctrl_chords_do_not_type_their_letter() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);

        for c in "hello".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .await
                .unwrap();
        }
        // Standard readline chords the composer does not implement. Each one
        // must be ignored, never inserted.
        for c in ['a', 'e', 'w', 'k', 'u', 'b', 'f', 'l', 'n', 'p'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
                .await
                .unwrap();
        }

        assert_eq!(app.context.input_buffer, "hello");
    }

    /// Ctrl+J is the composer's documented newline ("Ctrl+J newline" in the
    /// placeholder) and must keep working once the CONTROL guard lands.
    #[tokio::test]
    async fn ctrl_j_still_inserts_a_newline() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);

        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE))
            .await
            .unwrap();

        assert_eq!(app.context.input_buffer, "a\nb");
    }

    /// Shift+letter and AltGr-style composed input must still type — the
    /// guard has to reject CONTROL specifically, not "any modifier".
    #[tokio::test]
    async fn shift_and_alt_modified_chars_still_type() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);

        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT))
            .await
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::ALT))
            .await
            .unwrap();

        assert_eq!(app.context.input_buffer, "Aé");
    }

    #[tokio::test]
    async fn ctrl_c_in_streaming_sends_cancel_and_sets_cancelling_flag() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };

        let result = app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        assert_eq!(result, EventResult::Continue);
        let got = req_rx.recv().await.unwrap();
        assert!(matches!(got, TurnRequest::Cancel));
        if let AppState::Streaming { cancelling, .. } = &app.state {
            assert!(*cancelling);
        } else {
            panic!("state should remain Streaming during cancel");
        }
    }

    #[tokio::test]
    async fn ctrl_c_in_ready_state_transitions_to_quitting() {
        let (ctx, mut _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Ready;

        let result = app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();

        assert_eq!(result, EventResult::Quit);
        assert!(matches!(app.state, AppState::Quitting));
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;
    use crate::agent::events::AgentEvent;
    use crate::tui::context::TuiContext;

    fn make_app_with_context(ctx: TuiContext) -> TuiApp {
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config: crate::config::Config::default(),
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: crate::profile::paths::profile_dir("default"),
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_save_complete_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            clawhub_install_last_query: String::new(),
            clawhub_install_search_version: 0,
            clawhub_install_results_rx: None,
            clawhub_install_results_tx: None,
            clawhub_install_in_progress: None,
            clawhub_install_completion_rx: None,
            clawhub_install_completion_tx: None,
            skill_deps_install_in_progress: None,
            skill_deps_install_completion_rx: None,
            skill_deps_install_completion_tx: None,
            skill_deps_install_finished_at: None,
            skills_watcher: None,
            config_watcher: None,
            channel_supervisor: None,
            wizard_install_in_progress: false,
            wizard_installed_slugs: Vec::new(),
            info_panel: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
            login_gate: None,
            pending_approvals_rx: None,
            shell_blocks_this_turn: 0,
            autonomy_hint_shown_this_turn: false,
            pending_approval: None,
        }
    }

    #[tokio::test]
    async fn drain_events_chunk_appends_to_partial() {
        let (ctx, _req_rx, events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::from("prev "),
            tool_blocks: vec![],
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };
        events_tx
            .send(AgentEvent::Chunk("more".into()))
            .await
            .unwrap();

        app.drain_events();

        if let AppState::Streaming { partial, .. } = &app.state {
            assert_eq!(partial, "prev more");
        } else {
            panic!("expected Streaming");
        }
    }

    #[tokio::test]
    async fn drain_events_done_finalizes_turn_to_ready() {
        let (ctx, _req_rx, events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::from("answer"),
            tool_blocks: vec![],
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };
        events_tx
            .send(AgentEvent::Done {
                final_text: "answer".into(),
                cancelled: false,
            })
            .await
            .unwrap();

        app.drain_events();

        assert!(matches!(app.state, AppState::Ready));
        assert!(!app.context.messages.is_empty());
        assert_eq!(app.context.messages.last().unwrap().content, "answer");
    }

    #[tokio::test]
    async fn drain_events_done_cancelled_appends_marker_using_local_partial() {
        let (ctx, _req_rx, events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::from("partial text from chunks"),
            tool_blocks: vec![],
            cancelling: true,
            turn_started_at: std::time::Instant::now(),
        };
        // Agent emits Done with empty final_text on cancel — TUI must use local partial.
        events_tx
            .send(AgentEvent::Done {
                final_text: String::new(),
                cancelled: true,
            })
            .await
            .unwrap();

        app.drain_events();

        assert!(matches!(app.state, AppState::Ready));
        let last = app.context.messages.last().unwrap();
        assert!(last.content.contains("partial text"));
        assert!(last.content.contains("[cancelled]"));
    }

    #[tokio::test]
    async fn drain_events_tool_call_start_end_updates_blocks() {
        let (ctx, _req_rx, events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };
        events_tx
            .send(AgentEvent::ToolCallStart {
                id: "call-1".into(),
                name: "shell".into(),
                args: serde_json::json!({"cmd":"ls"}),
            })
            .await
            .unwrap();
        events_tx
            .send(AgentEvent::ToolCallEnd {
                id: "call-1".into(),
                ok: true,
                output_preview: "files".into(),
            })
            .await
            .unwrap();

        app.drain_events();

        if let AppState::Streaming { tool_blocks, .. } = &app.state {
            assert_eq!(tool_blocks.len(), 1);
            assert_eq!(tool_blocks[0].name, "shell");
            assert_eq!(tool_blocks[0].result, Some((true, "files".into())));
        } else {
            panic!("expected Streaming");
        }
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::tui::async_bridge::TurnRequest;
    use crate::tui::context::TuiContext;

    fn make_app_with_context(ctx: TuiContext) -> TuiApp {
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config: crate::config::Config::default(),
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: crate::profile::paths::profile_dir("default"),
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_save_complete_rx: None,
            setup_response_tx: None,
            scrollback_queue: Vec::new(),
            list_picker: None,
            clawhub_install_last_query: String::new(),
            clawhub_install_search_version: 0,
            clawhub_install_results_rx: None,
            clawhub_install_results_tx: None,
            clawhub_install_in_progress: None,
            clawhub_install_completion_rx: None,
            clawhub_install_completion_tx: None,
            skill_deps_install_in_progress: None,
            skill_deps_install_completion_rx: None,
            skill_deps_install_completion_tx: None,
            skill_deps_install_finished_at: None,
            skills_watcher: None,
            config_watcher: None,
            channel_supervisor: None,
            wizard_install_in_progress: false,
            wizard_installed_slugs: Vec::new(),
            info_panel: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            clear_terminal_request: false,
            first_run_wizard: None,
            login_gate: None,
            pending_approvals_rx: None,
            shell_blocks_this_turn: 0,
            autonomy_hint_shown_this_turn: false,
            pending_approval: None,
        }
    }

    #[tokio::test]
    async fn retry_in_ready_resubmits_last_user_message_and_streams() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("previous prompt").unwrap();
        app.context.append_assistant_message("old reply").unwrap();
        app.state = AppState::Ready;

        app.handle_command("retry").await.unwrap();

        assert!(matches!(app.state, AppState::Streaming { .. }));
        // The user message is retained; the old assistant reply is gone.
        assert_eq!(app.context.messages.len(), 1);
        assert_eq!(app.context.messages[0].content, "previous prompt");
        let req = req_rx.recv().await.expect("resubmit should dispatch");
        match req {
            TurnRequest::Submit(text) => assert_eq!(text, "previous prompt"),
            TurnRequest::Cancel => panic!("expected Submit, got Cancel"),
            TurnRequest::Reload(_) => panic!("expected Submit, got Reload"),
            TurnRequest::Compact { .. } => panic!("expected Submit, got Compact"),
        }
        assert!(app.context.last_error.is_none());
    }

    #[tokio::test]
    async fn retry_in_streaming_refuses_and_sets_last_error() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("prompt").unwrap();
        app.context.append_assistant_message("reply").unwrap();
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };

        app.handle_command("retry").await.unwrap();

        assert!(matches!(app.state, AppState::Streaming { .. }));
        assert!(
            req_rx.try_recv().is_err(),
            "no request should be dispatched while streaming"
        );
        let err = app.context.last_error.as_deref().unwrap_or("");
        assert!(err.contains("Cannot retry"));
    }

    #[tokio::test]
    async fn retry_with_no_history_sets_message_and_stays_ready() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Ready;

        app.handle_command("retry").await.unwrap();

        assert!(matches!(app.state, AppState::Ready));
        assert!(req_rx.try_recv().is_err());
        // CommandResult::Message now appends as a system chat message
        // (instead of the single-line error slot) so multi-line content
        // renders properly.
        let last_msg = app.context.messages.last().expect("system message");
        assert_eq!(last_msg.role, "system");
        assert!(
            last_msg.content.contains("No previous response"),
            "got {:?}",
            last_msg.content
        );
    }
}
