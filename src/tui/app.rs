use std::io::{self, IsTerminal, Stdout, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
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
    Terminal,
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

/// Why a spawned ClawHub install did not finish successfully.
///
/// Ambiguity is modelled apart from ordinary failure because it is not really
/// an error from where the user sits — it is a question. ClawHub namespaces
/// skills per publisher, so a slug several of them share resolves to a
/// candidate list, and the picker turns that list into rows instead of
/// printing it as a message. The publisher is never chosen automatically.
pub enum ClawhubInstallFailure {
    Ambiguous(Box<crate::skills::clawhub::AmbiguousSkill>),
    Other(String),
}

/// Build the picker row for one ClawHub search or listing result.
///
/// The row `key` is the install *reference*, not the slug: `@owner/slug`
/// whenever the endpoint reported a publisher, a bare slug when it did not.
/// Enter feeds that key straight to `install_one`, so qualifying it here is
/// what lets a search result install in one step — `/search` reports the
/// publisher, while the listing endpoint omits it and still has to resolve
/// through the ambiguity prompt.
/// Deepest cause in an error chain, sized for a one-row status.
///
/// The outermost link is our own `.context("GET clawhub search")`, which tells
/// the user what we were doing and nothing about why it failed — it read
/// `✗ search failed — GET clawhub search`. The root is the part they can act
/// on: a refused connection, a DNS failure, a timeout.
fn root_cause_of(e: &anyhow::Error) -> String {
    let root = e
        .chain()
        .last()
        .map_or_else(|| e.to_string(), ToString::to_string);
    let head = root.lines().next().unwrap_or("").trim();
    crate::tui::render::truncate_to_cols(head, 60)
}

fn clawhub_result_item(
    skill: &crate::skills::clawhub::ClawHubSkill,
    installed: &std::collections::HashMap<String, Option<String>>,
) -> crate::tui::widgets::ListPickerItem {
    let name = if skill.display_name.is_empty() {
        skill.slug.clone()
    } else {
        skill.display_name.clone()
    };
    // Show who published it. Without the handle, a search for a shared slug
    // renders rows that cannot be told apart — one of the four `weather`
    // results is a verbatim fork of another, identical name and summary,
    // differing only by publisher.
    let mut primary = if skill.owner_handle.is_empty() {
        name
    } else {
        format!("{name}  @{}", skill.owner_handle)
    };
    if skill.official {
        primary.push_str(" ✓");
    }
    // Listings report stars, search reports downloads, and neither reports
    // both — show whichever the endpoint actually returned rather than a
    // misleading zero for the other.
    if skill.stats.stars > 0 {
        primary = format!("{primary}  (★{})", skill.stats.stars);
    } else if skill.stats.downloads > 0 {
        primary = format!("{primary}  (↓{})", skill.stats.downloads);
    }
    // Whether this row is already on disk, and if so whose copy. Reinstalling
    // is refused by the gateway when another publisher holds the directory, so
    // a row offering an install that cannot happen is worse than a quiet one.
    match installed.get(&skill.slug) {
        None => {}
        Some(None) => primary.push_str("  · installed"),
        Some(Some(owner)) if owner == &skill.owner_handle => primary.push_str("  · installed"),
        Some(Some(owner)) => {
            use std::fmt::Write as _;
            let _ = write!(primary, "  · @{owner} installed");
        }
    }
    // Left whole: the picker fits it to the width it actually has, and a
    // second cap here only guaranteed the two would disagree. The old 90-char
    // cut also measured characters, which halves the budget for the CJK
    // summaries ClawHub returns.
    let secondary = crate::tui::render::flatten_to_row(&skill.summary);
    let key = if skill.owner_handle.is_empty() {
        skill.slug.clone()
    } else {
        format!("@{}/{}", skill.owner_handle, skill.slug)
    };
    crate::tui::widgets::ListPickerItem {
        key,
        primary,
        secondary,
        disabled: false,
    }
}

/// Build the picker rows offered when a slug turns out to be shared by
/// several publishers. Every key is fully qualified, so choosing a row
/// installs that publisher's copy without another round trip.
fn clawhub_candidate_items(
    ambiguous: &crate::skills::clawhub::AmbiguousSkill,
) -> Vec<crate::tui::widgets::ListPickerItem> {
    ambiguous
        .matches
        .iter()
        .map(|m| {
            // Annotate with install count and the official marker. Choosing
            // between bare handles is choosing blind, and this is the one
            // decision the flow refuses to make for the user — among four
            // `weather` publishers, one is a verbatim fork of the top one with
            // the same name and summary.
            let annotation = m.annotation();
            let primary = if annotation.is_empty() {
                format!("@{}", m.owner_handle)
            } else {
                format!("@{}  ({annotation})", m.owner_handle)
            };
            crate::tui::widgets::ListPickerItem {
                key: format!("@{}/{}", m.owner_handle, ambiguous.slug),
                primary,
                secondary: m.url.clone(),
                disabled: false,
            }
        })
        .collect()
}

/// A turn the TUI submitted to the agent actor and is still waiting on.
#[derive(Debug, Clone, Copy)]
pub struct SubmittedTurn {
    /// Store id of the user row this turn answers, when it has one. `None`
    /// for a resubmit that appended no row (`/continue`). This is how a
    /// refusal finds the row to take back: the turn names it, not "the last
    /// user row on screen", which a message submitted in the meantime has
    /// already replaced.
    pub row_id: Option<i64>,
    /// The agent refused this turn's message. The `Done` that ends it belongs
    /// to a turn that never ran, so it ends the turn without committing a
    /// reply — and it says so about THIS turn, whatever was submitted after it.
    pub refused: bool,
}

/// A reload the TUI sent to the agent actor and is still waiting on: what its
/// outcome is allowed to change, and nothing else.
///
/// The status bar and the saved config both name a model only once an agent is
/// running it, so neither moves before the matching `ReloadComplete` — and a
/// `ReloadFailed` (or an outcome belonging to an older reload) changes
/// neither. Reloads that change no model at all (a skill toggle, an autonomy
/// preset) are not held here: their outcome does nothing but the MCP/security
/// snapshots every `ReloadComplete` already carries.
///
/// One slot, not a queue: a reload asked for while another is in flight
/// supersedes the earlier one, and the superseded one is reported by nobody.
#[derive(Debug)]
pub struct PendingReload {
    /// Correlation id stamped on the `TurnRequest::Reload`. Reload ids are
    /// monotonic and every reload sent after this one carries this one's model,
    /// so an outcome whose id is at or past this one is this reload's verdict.
    pub id: u64,
    /// `provider:model`, the label the rebuilt agent runs. `None` for a reload
    /// whose config names no model, where the status bar stays as it is.
    pub model_label: Option<String>,
    /// Set by `/model` alone: the config the switch produced, which the agent
    /// has now been built from but which is on neither `self.config` nor disk.
    /// Its presence is also what makes a failure read "Model not changed" — a
    /// plain config reload changed no model and must not claim it did.
    pub unsaved_config: Option<Box<crate::config::Config>>,
}

/// Top-level TUI application.
// Many independent UI mode flags; grouping them into a sub-struct would not
// improve clarity and would churn every call site.
#[allow(clippy::struct_excessive_bools)]
pub struct TuiApp {
    pub state: AppState,
    pub context: TuiContext,
    pub command_registry: CommandRegistry,
    /// Anchor of an in-progress mouse drag over the chat pane — set on
    /// left-button Down, promoted into `context.selection` on the first Drag.
    /// `None` when no gesture is active.
    pub selection_drag_anchor: Option<super::selection::LineAnchor>,
    /// True once the active gesture has actually dragged (moved while held).
    /// A Down+Up with no drag is a plain click, which clears the selection.
    pub selection_dragged: bool,
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
    /// Set when the save-complete branch has already restarted for the save
    /// that `reload_config` is about to observe.
    ///
    /// The save branch restarts immediately (the point of this fix) and
    /// `reload_config` runs on Esc right after, re-reading the same
    /// configuration — the live drive showed two restart lines for one
    /// `/setup`, which costs the Telegram 409 window twice. A fingerprint memo
    /// cannot do this: `save()` encrypts credentials, so the config written to
    /// disk does not serialise identically to the one held in memory.
    pub channels_restarted_for_save: bool,

    /// The change a deferred listener restart is owed for.
    ///
    /// The wizard saves several sections in sequence; restarting per save
    /// would flap every listener once per provisioner, so the restart is
    /// deferred to the end rather than dropped. The variant says what
    /// changed, because the line the wizard prints at the end may only
    /// claim channels when channels were actually configured.
    pub pending_setup_change: SetupChange,
    /// The `--force` intent for the setup surfaces this session opens.
    ///
    /// Set from `TuiConfig::setup_force`, which the CLI fills from
    /// `rantaiclaw setup --force`. A plain `rantaiclaw chat` session leaves
    /// it false, so `/setup approvals` inside one does NOT force.
    pub setup_force: bool,
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
    /// Local slug currently being installed from ClawHub, plus the spinner
    /// frame index (advanced each render tick). `None` when no install is in
    /// flight. While `Some`, the picker title shows a Braille-spinner
    /// "Installing …" line that animates per tick.
    ///
    /// Holds the *slug*, not the `@owner/slug` reference the install was
    /// launched with: it doubles as the preselect key for the Skill picker
    /// that replaces this one on success, and those rows are keyed by the
    /// local manifest name.
    pub clawhub_install_in_progress: Option<(String, usize)>,
    /// Spinner frame for an in-flight ClawHub *search*, or `None` when none is
    /// running. The install path above already animates; searching did not,
    /// and it is the slower of the two.
    pub clawhub_search_frame: Option<usize>,
    /// Completion channel for spawned ClawHub install tasks. `Ok(slug)` on
    /// success. The render loop drains this each tick — when a result
    /// arrives, the install picker swaps to the Skill picker (success), asks
    /// which publisher was meant (ambiguous), or flips its title to an error
    /// string (anything else).
    pub clawhub_install_completion_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<Result<String, ClawhubInstallFailure>>>,
    pub clawhub_install_completion_tx:
        Option<tokio::sync::mpsc::UnboundedSender<Result<String, ClawhubInstallFailure>>>,
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
    /// Fingerprint of the `config.toml` bytes this app already accounts for.
    /// A watcher tick whose file content matches it is not an external edit —
    /// it is the write `/model`'s save just made, or a touch that changed
    /// nothing — so the reload it would trigger is skipped. Shared with that
    /// save task, which stamps it once the bytes are there; the gateway gates
    /// its own reloader on the same value. Behind a mutex because that task is
    /// not on this thread.
    ///
    /// Only `/model`'s save stamps it. A save this app makes for another reason
    /// (`/autonomy`'s preset) does not, so its tick still reloads the agent, as
    /// it did before this gate existed — suppressing that rebuild is a separate
    /// change with its own evidence, not a side effect of this one.
    pub config_fingerprint: Arc<std::sync::Mutex<String>>,
    /// Failure line for the `config.toml` save a `/model` switch triggers once
    /// the agent it waits for is running. The save runs off-thread, so the
    /// "Model set to" line is said before the write is known to have landed;
    /// this carries the other outcome back to `drain_events`, which says it.
    /// `None` when no switch save is outstanding.
    pub model_switch_save_rx: Option<tokio::sync::mpsc::UnboundedReceiver<String>>,
    /// Handle to the background channel runtime (Telegram/Discord/Slack/…
    /// listeners) spawned alongside the TUI. Held so a mid-session channel
    /// or skill change can cancel and respawn the runtime in place via
    /// `restart_channels` — newly-configured channels start polling and the
    /// rebuilt system prompt picks up new skills without closing the TUI.
    pub channel_supervisor: Option<ChannelSupervisor>,
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
    /// Deferred `/skill new` / `/skill edit`. Like `editor_request`, the work
    /// happens in `run_loop`, which owns the `Terminal` the handoff has to
    /// suspend and restore.
    pub skill_editor_request: Option<SkillEditRequest>,
    /// `true` when the run loop should wipe the terminal's screen and
    /// scrollback before the next render (e.g. after `/new`/`/clear`).
    /// Set by command handlers via `CommandResult::ClearTerminal` and
    /// consumed once the wipe is performed.
    pub clear_terminal_request: bool,
    /// Height (rows) the inline viewport was last (re)created with. The
    /// run-loop reconcile step recreates the terminal whenever the buffer's
    /// needed height diverges from this, giving the composer its grow/shrink.
    pub composer_viewport_rows: u16,
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
    /// Turns submitted to the actor and not yet ended, oldest first, each
    /// with the user row it answers. The actor answers turns in submission
    /// order, so the front is the turn in flight: a refusal takes back its own
    /// row, and the `Done` that follows is matched to the turn it ends even
    /// when another message was submitted while it ran.
    pub pending_turns: std::collections::VecDeque<SubmittedTurn>,
    /// Whether the bridge's actor holds an `Agent`. `false` only when the
    /// boot-time `Agent::from_config` failed and no `Reload` has healed the
    /// session since.
    ///
    /// The actor owns the agent; this records that one fact because the TUI
    /// cannot otherwise tell an error that ends a turn for good — there was
    /// no agent to run it, so no `Done` will ever follow — from the error a
    /// running turn sends before its own `Done`. `run_tui` clears it when the
    /// boot failed; a reload that rebuilds the agent sets it again.
    pub actor_has_agent: bool,
    /// Most-recent unresolved pending approval. When `Some`, the input
    /// box surfaces a single-key Y/N/A prompt and absorbs Y/N/A/Esc so
    /// the user doesn't have to type `/allow X`. Resolves via
    /// `security.pending().resolve_by_basename(...)` and clears here.
    pub pending_approval: Option<crate::security::PendingRequest>,
    /// The reload this app is waiting on, and what its outcome may change.
    /// An outcome whose id is at or past the pending id adopts the model label
    /// and writes the unsaved config; an older outcome moves nothing.
    pub pending_reload: Option<PendingReload>,
    /// Correlation id for the next `TurnRequest::Reload` this app sends.
    /// Monotonic and never reused, so an outcome can only ever match one
    /// request.
    pub reload_seq: u64,
    /// The `(provider, model)` each row of the open model picker stands for,
    /// in the picker's own row order. A row key is opaque (`model-row:{n}`),
    /// so this is where a chosen row's pair is read from.
    pub model_picker_entries: Option<Vec<super::widgets::ModelEntry>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetupTopicAction {
    TuiProvisioner(String),
    /// Drill down into a category sub-picker. The string is the
    /// category key (`core`, `channel`, `integration`, `runtime`,
    /// `routing`).
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

    /// Installed skills as `slug -> publisher handle`, for marking ClawHub
    /// rows. `Some(None)` means the directory is taken but the gateway kept no
    /// attribution — which reads as "installed", not as "not from ClawHub".
    ///
    /// Keyed by slug because that is the directory a second install would have
    /// to claim, and the gateway refuses to overwrite one publisher's copy
    /// with another's.
    fn installed_slug_owners(&self) -> std::collections::HashMap<String, Option<String>> {
        self.context
            .available_skills
            .iter()
            .filter_map(|s| {
                let slug = s.slug()?;
                let owner = s
                    .origin
                    .as_ref()
                    .and_then(|o| o.source.as_deref())
                    .and_then(|src| src.trim().strip_prefix('@'))
                    .and_then(|rest| rest.split('/').next())
                    .filter(|h| !h.is_empty())
                    .map(str::to_string);
                Some((slug, owner))
            })
            .collect()
    }

    /// Rows for the local skills picker.
    ///
    /// Delegates to the `/skills` command's builder rather than repeating it.
    /// This was a verbatim copy, and the two had already begun to disagree:
    /// the copy shown after an install is the one where knowing which
    /// publisher you just took code from matters most, and it was the one
    /// without that label.
    fn skill_picker_items(&self) -> Vec<crate::tui::widgets::ListPickerItem> {
        crate::tui::commands::skills::build_skill_items(&self.context.available_skills_with_status)
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
        // Per-profile sessions.db — `profile` is the active profile the TUI
        // launched under, so history never leaks across profiles.
        let db_path = profile.sessions_db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
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
        context.available_commands = command_registry.get_help_detailed();

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
            pending_setup_change: SetupChange::None,
            channels_restarted_for_save: false,
            setup_response_tx: None,
            setup_force: tui_config.setup_force,
            scrollback_queue: Vec::new(),
            selection_drag_anchor: None,
            selection_dragged: false,
            list_picker: None,
            clawhub_install_last_query: String::new(),
            clawhub_install_search_version: 0,
            clawhub_install_results_rx: None,
            clawhub_install_results_tx: None,
            clawhub_install_in_progress: None,
            clawhub_search_frame: None,
            clawhub_install_completion_rx: None,
            clawhub_install_completion_tx: None,
            skill_deps_install_in_progress: None,
            skill_deps_install_completion_rx: None,
            skill_deps_install_completion_tx: None,
            skill_deps_install_finished_at: None,
            skills_watcher: None,
            config_watcher: None,
            config_fingerprint: Arc::new(std::sync::Mutex::new(String::new())),
            model_switch_save_rx: None,
            channel_supervisor: None,
            info_panel: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            skill_editor_request: None,
            clear_terminal_request: false,
            composer_viewport_rows: INLINE_VIEWPORT_LINES,
            first_run_wizard: None,
            login_gate: None,
            pending_approvals_rx: None,
            shell_blocks_this_turn: 0,
            autonomy_hint_shown_this_turn: false,
            pending_turns: std::collections::VecDeque::new(),
            actor_has_agent: true,
            pending_approval: None,
            pending_reload: None,
            reload_seq: 0,
            model_picker_entries: None,
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
    /// step (`Manual → Smart → Strict → Manual`, **skipping `Off`**) and
    /// persist it to
    /// `<policy_dir>/{autonomy,command_allowlist}.toml`.
    ///
    /// Wired to `KeyCode::BackTab` (Shift+Tab). The `/autonomy` slash
    /// command goes through the same writer but with `force = true` (see
    /// `src/tui/commands/autonomy.rs`), so it regenerates the policy
    /// files and overwrites hand-edited `command_allowlist.toml`; only
    /// Shift+Tab preserves user edits.
    ///
    /// Two writes, and both are needed:
    ///
    /// - `write_policy_files(.., force = false)` creates the bundle on a fresh
    ///   profile and otherwise **preserves** hand-edited
    ///   `command_allowlist.toml` — a keybinding is one
    ///   keypress with no confirmation, so it must not clobber them.
    /// - `write_active_preset` then moves the active-preset marker, which lives
    ///   inside `autonomy.toml` and is therefore left untouched by the
    ///   `force = false` write above. Without it the cycle never advanced past
    ///   one step on any profile that already had the file.
    ///
    /// `runtime_allowlist.toml` (the user's `/allow X --persist` accretions)
    /// lives in a separate file and is untouched either way.
    fn cycle_autonomy_preset(&mut self) {
        use crate::approval::policy_writer::{self, PolicyPreset};
        let dir = self.profile.policy_dir();
        let current = policy_writer::read_active_preset(&dir).unwrap_or(PolicyPreset::Smart);
        // `next_cycled`, not `next`: the cycle skips `Off` ("no prompts").
        // Removing the approval gate entirely is an explicit act — `/autonomy
        // off` — not something one unconfirmed keypress can do.
        let next = current.next_cycled();
        // `force = false`: this path is a keybinding. Forcing rewrote the
        // policy files wholesale, clobbering hand-edited allowlists on an
        // accidental press — the function's own doc said so.
        let warning = match policy_writer::write_policy_files(&self.profile, next, false) {
            Ok(w) => w,
            Err(e) => {
                let _ = self
                    .context
                    .append_system_message(&format!("✗ Failed to switch autonomy mode: {e}"));
                return;
            }
        };
        // `write_policy_files` with `force = false` leaves an existing
        // `autonomy.toml` alone — deliberately, so an accidental press cannot
        // clobber hand-edited allowlists. But the active-preset marker lives in
        // that same file, so on any profile that has one (every configured
        // install) the write above moved nothing: `read_active_preset` returned
        // the same rung on the next press and the cycle never advanced past one
        // step, while `config.toml` drifted away from the marker.
        //
        // `write_active_preset` is the narrow write for exactly this — it
        // updates the marker and leaves `command_allowlist.toml` untouched.
        // The gateway already calls it after
        // an autonomy change for the same reason.
        if let Err(e) = policy_writer::write_active_preset(&dir, next) {
            let _ = self
                .context
                .append_system_message(&format!("✗ Failed to switch autonomy mode: {e}"));
            return;
        }
        // Propagate to config.toml + live agent. Without this step the
        // preset file changes but `SecurityPolicy.autonomy` keeps its
        // launch-time value (v0.6.49 bug — Off didn't actually disable
        // the gate). `apply_preset_to_config_and_reload` saves
        // config.toml synchronously via block_in_place and asks the
        // actor to rebuild the agent with the new policy.
        if let Err(e) = self.apply_preset_to_config_and_reload(next) {
            // Do NOT record or report the new level. It used to update
            // `autonomy_preset` and announce the new rung even when the live
            // reload failed, so the status bar asserted a level that was not in
            // force — the most dangerous direction for this particular lie.
            let _ = self.context.append_system_message(&format!(
                "✗ Autonomy unchanged — the policy files were written but the live reload failed: {e}. \
                 Still running as {}.",
                current.label()
            ));
            return;
        }
        self.context.autonomy_preset = Some(next);
        let _ = self.context.append_system_message(&format!(
            "⚙ Autonomy mode → {} ({}). Shift+Tab cycles Manual→Smart→Strict · `/autonomy off` to disable prompts.",
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
        //
        // The preset change travels with the model a `/model` switch is still
        // waiting to land, exactly as it travels from every other sender
        // (`with_pending_model`). This reload is the last one the actor sees,
        // so it decides what the next message is answered with.
        let id = self.next_reload_id();
        let req_tx = self.context.req_tx.clone();
        let config = self.with_pending_model(self.config.clone());
        tokio::spawn(async move {
            let _ = req_tx
                .send(crate::tui::TurnRequest::Reload {
                    id,
                    config: Box::new(config),
                })
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
        // Resolve by ID, and grant only after it resolves.
        //
        // This used to widen the allowlist FIRST and then resolve by basename —
        // which returns nothing when two requests share one. The agent loop runs
        // tool calls in parallel (this file says so), so two pending `curl`
        // calls are ordinary: pressing `A` permanently allowlisted `curl`,
        // cleared the prompt, resolved neither call, and hung the turn.
        let resolved = pending.resolve(req.id, decision);

        let persist_flag = matches!(decision, crate::security::Decision::Persist);
        if resolved
            && matches!(
                decision,
                crate::security::Decision::Session | crate::security::Decision::Persist
            )
        {
            if let Err(e) = security.add_runtime_command(&basename, persist_flag) {
                tracing::warn!(
                    target: "tui",
                    error = %e,
                    basename = %basename,
                    "inline approval add_runtime_command failed"
                );
            }
        }

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
            // Was "no longer pending (timed out or already resolved)", which
            // was false in the case that actually produced it: the request WAS
            // pending, the basename lookup just could not tell two of them
            // apart. Nothing was granted either, since the grant now follows
            // the resolve.
            _ => format!(
                "⚠ Could not resolve the approval for `{basename}` — it is no longer registered. \
                 Nothing was added to the allowlist."
            ),
        };
        let _ = self.context.append_system_message(&msg);
        self.scrollback_queue.push(("system".into(), msg));

        // Concurrent blocked calls: an approve keeps the turn running, so pull
        // the next still-queued request into the box instead of leaving it
        // stranded off-screen. Skip the request we just resolved BY ID — its
        // snapshot entry is cleared asynchronously (when its awaiting future
        // unwinds), so `list()` can still report it for a tick. Skipping by
        // basename also dropped a *different* pending call that happened to
        // share one, which is the same conflation the resolve had. A Deny
        // cancels the whole turn, so don't resurface anything there.
        if matches!(
            decision,
            crate::security::Decision::Session | crate::security::Decision::Persist
        ) {
            // Requeued whether or not this one resolved: if it did not, the
            // box would otherwise be left empty with calls still blocked.
            self.pending_approval = pending.list().into_iter().find(|r| r.id != req.id);
        }
    }

    /// Replace the input buffer with the highlighted command name.
    fn complete_selected_command(&mut self) {
        if let Some(name) = self.autocomplete.selected() {
            self.context.input_buffer = format!("{name} ");
            self.context.cursor_to_end();
            self.autocomplete.hide();
        }
    }

    /// Route a paste (bracketed `Event::Paste` OR a coalesced key burst) into
    /// the composer. Normalises line breaks, then collapses a tall paste to a
    /// `[Pasted text #N +M lines]` placeholder (real content re-expanded at
    /// submit) or lands a short paste inline.
    fn apply_paste_text(&mut self, text: &str) {
        let normalized = normalize_line_breaks(text);
        if estimated_paste_rows(&normalized) > crate::tui::context::PASTE_PLACEHOLDER_MIN_ROWS {
            let marker = self.context.register_paste(normalized);
            self.context.paste_at_cursor(&marker);
        } else {
            self.context.paste_at_cursor(&normalized);
        }
        self.context.exit_history_navigation();
        self.refresh_autocomplete();
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
                    // Bracketed paste delivered as one payload — route through
                    // the shared helper (same path the key-burst coalescer uses).
                    self.apply_paste_text(&text);
                }
                Ok(EventResult::Continue)
            }
            // Mouse wheel scrolls the chat pane (same `scroll_offset` as
            // PgUp/PgDn: up counts lines back from the newest, 0 sticks to the
            // bottom; render clamps to the top). Left-button Down/Drag/Up is
            // the selection gesture: drag highlights display lines, Ctrl+C
            // copies, a plain click clears. Only when the chat composer
            // owns the screen — a modal/overlay/approval keeps its own view.
            Event::Mouse(me)
                if !self.modal_active()
                    && self.overlay.is_none()
                    && self.pending_approval.is_none() =>
            {
                match me.kind {
                    MouseEventKind::ScrollUp => {
                        self.context.scroll_offset = self.context.scroll_offset.saturating_add(3);
                    }
                    MouseEventKind::ScrollDown => {
                        self.context.scroll_offset = self.context.scroll_offset.saturating_sub(3);
                    }
                    MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        self.selection_dragged = false;
                        self.selection_drag_anchor = self.chat_anchor_at(me.column, me.row);
                    }
                    MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                        if let Some(anchor) = self.selection_drag_anchor {
                            // Clamp the drag row into the pane so dragging
                            // past the border keeps extending the selection.
                            if let Some(head) = self.chat_anchor_at_clamped(me.column, me.row) {
                                self.selection_dragged = true;
                                self.context.selection =
                                    Some(super::selection::Selection { anchor, head });
                            }
                        }
                    }
                    MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                        // Down+Up with no drag in between = a plain click:
                        // clear any existing highlight (the Windows model).
                        if !self.selection_dragged {
                            self.context.selection = None;
                        }
                        self.selection_drag_anchor = None;
                        self.selection_dragged = false;
                    }
                    _ => {}
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
                KeyCode::Char('y' | 'Y') => {
                    self.resolve_pending_approval(crate::security::Decision::Session);
                    return Ok(EventResult::Continue);
                }
                KeyCode::Char('a' | 'A') => {
                    self.resolve_pending_approval(crate::security::Decision::Persist);
                    return Ok(EventResult::Continue);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
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
            // Ctrl+C → copy the active selection if one exists (the Windows
            // Terminal mental model: Ctrl+C with a highlight copies, without
            // one it keeps its control meaning). Selection FIRST — an
            // operator with a highlight active must never cancel their
            // running turn by copying. No selection → cancel streaming turn
            // if active; otherwise quit — both arms unchanged.
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(sel) = self.context.selection.take() {
                    self.selection_drag_anchor = None;
                    self.selection_dragged = false;
                    self.copy_selection(sel);
                    return Ok(EventResult::Continue);
                }
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
                // Only the browser searches. The publisher prompt is a fixed
                // answer to a fixed question, so a search there would swap its
                // rows out from under a title still asking which publisher —
                // and typing returns focus to the search box, so a single
                // keystroke used to be enough to trigger it.
                if kind == ListPickerKind::ClawhubInstall && focus == Focus::Search {
                    self.spawn_clawhub_search(&query);
                    // After firing, move focus to the list so the
                    // next Enter installs (when results arrive).
                    if let Some(p) = self.list_picker.as_mut() {
                        p.focus = Focus::List;
                    }
                    return Ok(EventResult::Continue);
                }
                if matches!(
                    kind,
                    ListPickerKind::ClawhubInstall | ListPickerKind::ClawhubPublisher
                ) {
                    // The row key is the install *reference*: `@owner/slug`
                    // whenever the endpoint told us who published it, a bare
                    // slug otherwise. The local slug is derived from it
                    // separately — it names the directory on disk and is the
                    // preselect key for the Skill picker shown afterwards,
                    // which keys rows by local manifest name, not by
                    // reference.
                    let reference = match self
                        .list_picker
                        .as_ref()
                        .and_then(|p| p.current().map(|i| i.key.clone()))
                    {
                        Some(s) => s,
                        None => return Ok(EventResult::Continue),
                    };
                    let slug = crate::skills::clawhub::parse_skill_ref(&reference)
                        .map_or_else(|_| reference.clone(), |parsed| parsed.slug);
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
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<
                        Result<String, ClawhubInstallFailure>,
                    >();
                    self.clawhub_install_completion_rx = Some(rx);
                    self.clawhub_install_completion_tx = Some(tx.clone());
                    self.clawhub_install_in_progress = Some((slug.clone(), 0));
                    let profile = self.profile.clone();
                    let reference_for_task = reference.clone();
                    tokio::spawn(async move {
                        let send = match crate::skills::clawhub::install_one(
                            &profile,
                            &reference_for_task,
                        )
                        .await
                        {
                            Ok(()) => Ok(slug),
                            // An ambiguous slug comes back as a typed error
                            // carrying every candidate publisher; keep it
                            // structured so the picker can offer them as rows
                            // instead of flattening it into a message.
                            Err(e) => Err(
                                match e.downcast::<crate::skills::clawhub::AmbiguousSkill>() {
                                    Ok(ambiguous) => {
                                        ClawhubInstallFailure::Ambiguous(Box::new(ambiguous))
                                    }
                                    Err(other) => {
                                        ClawhubInstallFailure::Other(format!("{other:#}"))
                                    }
                                },
                            ),
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
            KeyCode::Char('e')
                if self.list_picker.as_ref().is_some_and(|p| {
                    p.kind == crate::tui::widgets::ListPickerKind::Skill
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                }) =>
            {
                self.toggle_selected_skill().await;
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
            KeyCode::Char('r' | 'R')
                if self
                    .info_panel
                    .as_ref()
                    .is_some_and(|p| p.cron_job_id.is_some()) =>
            {
                self.cron_panel_action('r');
                return Ok(EventResult::Continue);
            }
            KeyCode::Char('p' | 'P')
                if self
                    .info_panel
                    .as_ref()
                    .is_some_and(|p| p.cron_job_id.is_some()) =>
            {
                self.cron_panel_action('p');
                return Ok(EventResult::Continue);
            }
            KeyCode::Char('d' | 'D')
                if self
                    .info_panel
                    .as_ref()
                    .is_some_and(|p| p.cron_job_id.is_some()) =>
            {
                self.cron_panel_action('d');
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
            // Also gated on there being no approval on screen and no turn in
            // flight. The approval handler requires unmodified keys and
            // `BackTab` carries SHIFT, so this arm used to fire from INSIDE an
            // open approval prompt: the operator is looking at a gate while
            // removing it, and one of the rungs is "no prompts".
            KeyCode::BackTab
                if self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none()
                    && self.pending_approval.is_none()
                    && !matches!(self.state, AppState::Streaming { .. }) =>
            {
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
            KeyCode::Tab | KeyCode::Right if self.overlay.is_some() => {
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
            // Readline editing chords (Claude-Code parity), line-based. These
            // must precede the plain-Char arm below, which excludes CONTROL.
            // Ctrl+A/E move to line start/end; Ctrl+U/K kill to line start/end;
            // Ctrl+W deletes the word behind the cursor.
            KeyCode::Char('a')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none() =>
            {
                self.context.cursor_line_start();
            }
            KeyCode::Char('e')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none() =>
            {
                self.context.cursor_line_end();
            }
            KeyCode::Char('u')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none() =>
            {
                self.context.kill_to_line_start();
                self.context.exit_history_navigation();
                self.refresh_autocomplete();
            }
            KeyCode::Char('k')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none() =>
            {
                self.context.kill_to_line_end();
                self.context.exit_history_navigation();
                self.refresh_autocomplete();
            }
            KeyCode::Char('w')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none() =>
            {
                self.context.delete_word_before();
                self.context.exit_history_navigation();
                self.refresh_autocomplete();
            }
            // Regular character input — insert at the cursor.
            // Plain text input. The CONTROL check is load-bearing: crossterm
            // reports an unhandled chord like Ctrl+U as `Char('u')` with the
            // CONTROL modifier, so without it every readline chord the app
            // does not implement typed its own letter into the buffer
            // (Ctrl+A/E/W/K/U on `hello` produced `helloaewku`). Chords we do
            // implement — Ctrl+C/D/J/G and the A/E/U/K/W arms above — match
            // first. SHIFT and ALT must still pass through: they carry text.
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
            // Up/Down move the caret between VISUAL rows of a multi-line
            // composer (editor-style), including soft-wrapped rows of a single
            // long line. Only when the caret is already on the first/last row
            // does Up/Down fall back to history: Up recalls an older submitted
            // prompt, Down recalls a newer one (restoring the stashed draft past
            // the newest). A single-row input therefore maps Up/Down straight to
            // history. PgUp/PgDn scroll the chat pane.
            KeyCode::Up if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                match visual_cursor_move(
                    &self.context.input_buffer,
                    self.context.cursor_pos,
                    self.context.last_composer_width,
                    true,
                ) {
                    Some(pos) => self.context.cursor_pos = pos,
                    None => {
                        if let Some(text) = self.context.history_recall_older() {
                            self.context.input_buffer = text;
                            self.context.cursor_to_end();
                            self.refresh_autocomplete();
                        }
                    }
                }
            }
            KeyCode::Down if self.setup_overlay.is_none() && self.first_run_wizard.is_none() => {
                match visual_cursor_move(
                    &self.context.input_buffer,
                    self.context.cursor_pos,
                    self.context.last_composer_width,
                    false,
                ) {
                    Some(pos) => self.context.cursor_pos = pos,
                    None => {
                        if let Some(text) = self.context.history_recall_newer() {
                            self.context.input_buffer = text;
                            self.context.cursor_to_end();
                            self.refresh_autocomplete();
                        }
                    }
                }
            }
            // PgUp/PgDn scroll the chat pane (in fullscreen the terminal's own
            // scrollback is gone, so this is how older messages are reached).
            // `scroll_offset` counts items up from the newest; 0 sticks to the
            // bottom and is reset on submit/`/new`.
            KeyCode::PageUp
                if self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none()
                    && self.list_picker.is_none()
                    && self.info_panel.is_none() =>
            {
                // Scroll up by one screenful of chat lines; render clamps the
                // upper bound to the top of the buffer.
                let page = self.context.last_chat_rows.max(1);
                self.context.scroll_offset = self.context.scroll_offset.saturating_add(page);
            }
            KeyCode::PageDown
                if self.setup_overlay.is_none()
                    && self.first_run_wizard.is_none()
                    && self.list_picker.is_none()
                    && self.info_panel.is_none() =>
            {
                let page = self.context.last_chat_rows.max(1);
                self.context.scroll_offset = self.context.scroll_offset.saturating_sub(page);
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
            // Esc with an active text selection clears the highlight and
            // stops — deliberately BEFORE the streaming-cancel arm below, so
            // the first Esc drops the selection and a second Esc cancels the
            // turn. Still after every modal handler: closing a picker /
            // overlay / wizard always wins over both.
            KeyCode::Esc if self.context.selection.is_some() => {
                self.context.selection = None;
                self.selection_drag_anchor = None;
                self.selection_dragged = false;
                return Ok(EventResult::Continue);
            }
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
            // Typed text narrows a filterable choose (the model list). Space is
            // left to the multi-select toggle, so it only types into a single
            // choose's filter.
            KeyCode::Char(c)
                if self.setup_overlay.as_ref().is_some_and(|o| {
                    o.active_choose()
                        .is_some_and(|ch| ch.filterable && (c != ' ' || !ch.multi))
                }) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.push_filter_char(c);
                }
            }
            KeyCode::Backspace
                if self
                    .setup_overlay
                    .as_ref()
                    .is_some_and(|o| o.active_choose().is_some_and(|ch| ch.filterable)) =>
            {
                if let Some(o) = self.setup_overlay.as_mut() {
                    o.pop_filter_char();
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
                        // Clear the wizard FIRST: the deferred restart below is
                        // only deferred because a wizard was running.
                        self.first_run_wizard = None;
                        // Reload config so the freshly-saved provisioner
                        // sections take effect in the running TUI.
                        if let Err(e) = self.reload_config() {
                            tracing::warn!("failed to reload config after wizard: {}", e);
                        }
                        // The wizard saves several channels in sequence, so each
                        // save deferred its restart to here rather than flapping
                        // every listener once per provisioner. `reload_config`
                        // may already have restarted; only fire if it did not.
                        //
                        // The line names what actually changed. A run that
                        // configured no channel but did save a policy still
                        // owes a restart — the listeners have to run under the
                        // new policy — and it must not claim channels.
                        match std::mem::take(&mut self.pending_setup_change) {
                            SetupChange::None => {}
                            SetupChange::Channels => {
                                let msg = "✓ Channels configured — starting listener(s) now. \
                                           `/channels` shows the current state."
                                    .to_string();
                                let _ = self.context.append_system_message(&msg);
                                self.scrollback_queue.push(("system".to_string(), msg));
                                self.restart_channels();
                            }
                            SetupChange::PolicyOnly => {
                                let msg = "✓ Approval policy applied — restarting listener(s) \
                                           now so it takes effect."
                                    .to_string();
                                let _ = self.context.append_system_message(&msg);
                                self.scrollback_queue.push(("system".to_string(), msg));
                                self.restart_channels();
                            }
                        }
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
        // A new submission supersedes any transient notice (copy feedback).
        self.context.status_notice = None;
        // Swap any `[Pasted text #N +M lines]` placeholders back for their real
        // content before the message is read or sent. Must precede the
        // empty-check: a buffer holding only a placeholder is not empty once
        // expanded.
        let raw = self.context.expand_pending_pastes(raw);
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
        let row_id = self.context.append_user_message(&text)?;
        // This turn answers this row. The queue is what ties a refusal, and
        // the `Done` that follows it, to the message they are about — a turn
        // submitted while another one is still in flight must not be mistaken
        // for either.
        self.pending_turns.push_back(SubmittedTurn {
            row_id: Some(row_id),
            refused: false,
        });
        // Commit user message to scrollback so it appears inline like
        // Hermes/Claude-Code chat output.
        self.scrollback_queue
            .push(("user".to_string(), text.clone()));

        // A reseed held back from an earlier command goes first, so this
        // prompt is answered from the session the screen shows.
        self.context.flush_pending_reseed().await;
        // Dispatch to the actor. If the bridge is closed (e.g. actor has
        // exited), surface a visible error but do not propagate — the TUI
        // should remain responsive so the user can /quit cleanly.
        if let Err(e) = self
            .context
            .req_tx
            .send(TurnRequest::Submit {
                text,
                conversation_id: self.context.conversation_scope(),
            })
            .await
        {
            // Nothing will ever answer this turn, so it must not sit in the
            // queue and be mistaken for a later one.
            self.pending_turns.pop_back();
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

        // A `/model` switch's config.toml save runs off-thread, so a write that
        // fails arrives here after the "Model set to" line was already said.
        // Said once, as an error: the agent really is on the new model, but the
        // file on disk is not, and only saying so keeps the two readable.
        let mut save_failures: Vec<String> = Vec::new();
        let mut save_task_gone = false;
        if let Some(rx) = self.model_switch_save_rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(line) => save_failures.push(line),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        save_task_gone = true;
                        break;
                    }
                }
            }
        }
        if save_task_gone {
            self.model_switch_save_rx = None;
        }
        for line in save_failures {
            self.context.last_error = Some(line.clone());
            let _ = self.context.append_system_message(&line);
            self.scrollback_queue.push(("system".to_string(), line));
        }

        // Pending-approval requests from the shell tool. Each new
        // request surfaces as a system message with the exact slash
        // commands the user can type to resolve it.
        if let Some(rx) = self.pending_approvals_rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(req) => {
                        // The FULL command goes here. The pane below
                        // truncates its preview at 200 characters and
                        // justifies that by saying the whole thing was
                        // shown in scrollback — which was not true: this
                        // line printed only the basename, so a long
                        // command was approved without ever being shown
                        // in full anywhere.
                        let line = format!(
                            "🔒 awaiting decision on `{}` (press Y/A/N or Esc)\n    {}",
                            req.basename,
                            req.full_command.replace('\n', "\n    ")
                        );
                        let _ = self.context.append_system_message(&line);
                        self.scrollback_queue.push(("system".into(), line));
                        // Surface this request in the boxed prompt only when
                        // none is already open — clobbering a still-unresolved
                        // one would strand it off-screen. The agent loop can run
                        // tool calls in parallel, so several shell commands may
                        // block at once; the extras stay in the registry and pop
                        // into the box as each is resolved (see
                        // resolve_pending_approval's advance step). `/allowlist`
                        // lists everything queued meanwhile. The TUI registry has
                        // no timeout (PendingApprovals::default), so nothing
                        // auto-denies — the queue drains only by user action.
                        if self.pending_approval.is_none() {
                            self.pending_approval = Some(req);
                        }
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                        // Missed some; user can /allowlist to see what's still pending.
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        self.pending_approvals_rx = None;
                        break;
                    }
                }
            }
        }
        if let Some(rx) = &mut self.setup_event_rx {
            while let Ok(ev) = rx.try_recv() {
                if let Some(o) = &mut self.setup_overlay {
                    o.handle_event(ev);
                }
            }
        }
        self.drain_skill_reload_events();
        self.drain_config_reload_events();
        // ClawHub install picker — async search results stream in via a
        // background task. Drain on each render tick so newly-arrived
        // results land between user actions, not just after the next key.
        self.drain_clawhub_search_results();
        // Draining first means a result that just landed clears the spinner
        // this frame instead of showing one more tick of "searching" after the
        // rows have already changed.
        self.tick_clawhub_search();
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
            // Compare BEFORE the swap. The swap itself is deliberate — the next
            // provisioner's clone must see this provisioner's writes — and
            // moving it reintroduces the bug it was added to fix.
            let change = setup_change(&self.config, cfg);
            // Save succeeded — swap in the new config NOW so the next
            // provisioner's clone sees this provisioner's writes.
            self.config = cfg.clone();

            // Restart from here, not only from `reload_config`: the documented
            // flow is `/setup channels` → picker → provisioner, and it used to
            // leave the live runtime untouched for the rest of the session
            // unless the operator happened to press Esc. Gated on an actual
            // diff, because `restart_channels` re-binds every listener and
            // firing it on a no-op save costs a Telegram 409 window.
            //
            // During the first-run wizard several channels are saved in
            // sequence; the restart is deferred to the end of the wizard so the
            // listeners are not flapped once per provisioner.
            if change != SetupChange::None {
                if self.first_run_wizard.is_some() {
                    // Keep the strongest claim: a wizard that saved a channel
                    // after a policy change may still say it configured one.
                    self.pending_setup_change = self.pending_setup_change.max(change);
                } else {
                    // The line names what changed. A save that only moved the
                    // policy still restarts the listeners — they have to run
                    // under it — but it saved no channel settings.
                    let msg = match change {
                        SetupChange::Channels => {
                            "✓ Channel settings saved — restarting listener(s) now. \
                             `/channels` shows the current state."
                                .to_string()
                        }
                        _ => "✓ Approval policy saved — restarting listener(s) now so it \
                              takes effect."
                            .to_string(),
                    };
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".to_string(), msg));
                    self.channels_restarted_for_save = true;
                    self.restart_channels();
                }
            }
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
                    let (rows, heading, disabled) = super::first_run_wizard::channel_options_full();
                    w.open_picker_with_disabled(rows, heading, disabled);
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
        let configured = crate::channels::configured_channel_count(&self.config);
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
            // Both early returns used to leave the auto-start state at whatever
            // it last was — typically `Starting`, which renders as "running"
            // after five seconds. `/channels` then said "polling" for a runtime
            // that had been torn down.
            crate::channels::auto_start_state::mark_terminated();
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
            crate::channels::auto_start_state::mark_terminated();
            return;
        }

        let cfg = self.config.clone();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let task_token = shutdown.clone();
        let handle = tokio::spawn(async move {
            // A panic inside this task reached neither arm of the match below,
            // so the state stayed `Starting` — which renders as "running"
            // forever. Catching it here maps a panic to the same failed state
            // an `Err` produces.
            let body = std::panic::AssertUnwindSafe(async {
                // Drain the previous runtime before starting the new one so the
                // old and new listeners don't fight over the same backend
                // (Telegram getUpdates returns 409 to a second concurrent
                // poller). Cancel + bounded await guarantees the old long-poll
                // has released first. The bound covers the runtime's own drain:
                // a WhatsApp Web connection stays open until dispatch has sent
                // its restart notices, and two connections on one session
                // conflict.
                if let Some(prev) = prev {
                    prev.shutdown.cancel();
                    let _ = tokio::time::timeout(
                        crate::channels::CHANNEL_RUNTIME_STOP_TIMEOUT,
                        prev.handle,
                    )
                    .await;
                }
                crate::channels::auto_start_state::mark_starting();
                match crate::channels::start_channels_with_cancellation(
                    cfg, task_token, None, None, None,
                )
                .await
                {
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
            let outcome = Box::pin(futures::FutureExt::catch_unwind(body)).await;
            if outcome.is_err() {
                crate::channels::auto_start_state::mark_failed(
                    "the channel runtime panicked; channels are not running".to_string(),
                );
            }
        });
        self.channel_supervisor = Some(ChannelSupervisor { shutdown, handle });
    }

    /// Apply a user-driven model switch from `/model`. `provider` and `model`
    /// arrive as two values: a picker row carries them apart (a model id may
    /// hold `/` and a base URL its own `:`, so joining them into one string and
    /// splitting it again loses either), and a typed `/model <provider:model>`
    /// is split once here by `split_model_target`.
    ///
    /// Nothing is written until the agent has been rebuilt from this config.
    /// The switch asks the actor for a rebuild and waits: `self.config`, the
    /// file and the status bar all move only on the `ReloadComplete` that
    /// answers THIS reload, and a `ReloadFailed` leaves all three as they were.
    fn apply_model_selection(&mut self, provider: Option<&str>, model: &str) {
        // The config this switch would produce. Nothing is written until the
        // build check below has passed on it.
        let mut next = self.config.clone();
        if let Some(provider) = provider {
            next.default_provider = Some(provider.to_string());
        }
        next.default_model = Some(model.to_string());

        // Build the provider the new config would use, through the same factory
        // and argument order `Agent::from_config_with_observer` uses. A target the
        // agent cannot be built with is refused here: nothing is saved, the
        // status bar keeps the model the agent runs, and no Reload is sent.
        if let Err(e) = check_model_buildable(&next) {
            let msg = format!("Model not changed: {e}");
            self.context.last_error = Some(msg.clone());
            let _ = self.context.append_system_message(&msg);
            self.scrollback_queue.push(("system".to_string(), msg));
            return;
        }

        let id = self.next_reload_id();
        let model_label = model_label_of(&next);
        // The config the agent is being built from is decrypted in memory (the
        // same one the provisioner save path clones), so the `Reload` hands it
        // usable credentials — the invariant `reload_config`'s decrypt pass
        // exists to protect.
        let config = Box::new(next);
        self.pending_reload = Some(PendingReload {
            id,
            model_label: Some(model_label),
            unsaved_config: Some(config.clone()),
        });
        let req_tx = self.context.req_tx.clone();
        tokio::spawn(async move {
            let _ = req_tx
                .send(crate::tui::TurnRequest::Reload {
                    id,
                    config: config.clone(),
                })
                .await;
        });
    }

    /// Correlation id for the next `TurnRequest::Reload`. Monotonic and never
    /// reused, so an outcome can only ever be matched to one request.
    fn next_reload_id(&mut self) -> u64 {
        self.reload_seq += 1;
        self.reload_seq
    }

    /// `base` with the model a `/model` switch is still waiting to land, when
    /// one is in flight.
    ///
    /// While that switch waits for its own `ReloadComplete`, its model is on
    /// neither `self.config` nor the file — it exists only in
    /// `pending_reload.unsaved_config`. The actor is FIFO and builds its agent
    /// from the config each `Reload` carries, so a reload sent in that window by
    /// any other path (`/autonomy`, a skill toggle, the skills watcher, the
    /// config watcher) that carried the config as it stands would put the agent
    /// back on the PRE-switch model — after the switch's own outcome already
    /// moved the label and wrote the file. `base` is the sender's own config,
    /// so its own changes stay; only the model comes from the switch.
    fn with_pending_model(&self, base: crate::config::Config) -> crate::config::Config {
        let Some(switch) = self
            .pending_reload
            .as_ref()
            .and_then(|pending| pending.unsaved_config.as_ref())
        else {
            return base;
        };
        let mut base = base;
        if let Some(provider) = switch.default_provider.as_deref() {
            base.default_provider = Some(provider.to_string());
        }
        if let Some(model) = switch.default_model.as_deref() {
            base.default_model = Some(model.to_string());
        }
        base
    }

    /// Whether a `/model` switch is still waiting on the rebuild it asked for.
    fn model_switch_in_flight(&self) -> bool {
        self.pending_reload
            .as_ref()
            .is_some_and(|pending| pending.unsaved_config.is_some())
    }

    /// Save a `/model` switch's config, now that an agent has been built from
    /// it, and record the bytes written so the config watcher's tick for this
    /// own write is recognised and skipped (`drain_config_reload_events`).
    ///
    /// The save runs off-thread, so the "Model set to" line is said before the
    /// write is known to have landed. A failure is not swallowed into the log:
    /// the task hands the line to `model_switch_save_rx` and `drain_events`
    /// says it. The in-session switch stands — an agent is running that model —
    /// so the line says exactly that, and names the file that is stale.
    ///
    /// `save()` takes `&self` and encrypts into a clone, so `self.config` keeps
    /// the decrypted credentials the provisioner paths read.
    fn persist_model_switch(&mut self, config: crate::config::Config, label: String) {
        let fingerprint = Arc::clone(&self.config_fingerprint);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.model_switch_save_rx = Some(rx);
        tokio::spawn(async move {
            if let Err(e) = config.save().await {
                let _ = tx.send(format!(
                    "⚠ Model set to {label} for this session, but saving config.toml \
                     failed: {e:#}. It keeps the model it had; the next save \
                     (/setup, the wizard) writes this one."
                ));
                tracing::warn!("failed to persist model change: {e:#}");
                return;
            }
            let written = crate::config::fingerprint::fingerprint_file(&config.config_path);
            *fingerprint.lock().unwrap_or_else(|e| e.into_inner()) = written;
        });
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
        // because the request builder rejects the malformed header.
        //
        // One shared pass with `Config::load_or_init` — this used to be a
        // hand-copy of that list, and the copies drifted twice (KB keys,
        // then `provider_api_keys`, which 401'd every provider call after
        // a config-watcher reload until #565). The shared function also
        // closes the two gaps the copy still had: the Telegram bot token
        // (handed to `restart_channels` below) and skill literal keys.
        let rantaiclaw_dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let store = crate::security::SecretStore::new(&rantaiclaw_dir, config.secrets.encrypt);
        crate::config::schema::decrypt_config_secrets(&store, &mut config)?;
        config.apply_env_overrides();
        // A `/model` switch in flight outranks what the file says: its model is
        // on neither `self.config` nor the file until its own rebuild lands, so
        // rebuilding the agent from the bytes just read would take the agent
        // back to the pre-switch model under a status bar the switch has not
        // moved yet. Everything else the file changed is kept — a hand edit
        // during the switch window still reaches the agent this way.
        let reload_config = self.with_pending_model(config.clone());
        // The status-bar model label follows this reload, not the file read
        // above: an operator (or a wizard run that switches provider, e.g.
        // openrouter → minimax) must not see the new model named until an
        // agent is actually running it, and a rebuild that fails must leave
        // the label on the model the agent still answers with.
        let model_label = model_label_of(&reload_config);
        let id = self.next_reload_id();
        // One pending slot, and a switch in flight holds it: taking it here
        // would discard the operator's `/model` with its `unsaved_config`, and
        // the switch's `ReloadComplete` would then match nothing — the file
        // read below would silently win over what the operator just typed. The
        // switch keeps the slot, so its own outcome still adopts it and writes
        // the file; this reload's outcome only moves the label when no switch
        // is waiting, as before.
        if !self.model_switch_in_flight() {
            self.pending_reload = Some(PendingReload {
                id,
                model_label: (!model_label.is_empty()).then_some(model_label),
                // The file already holds this config; `/model` alone writes.
                unsaved_config: None,
            });
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
        self.context.workspace_dir = Some(config.workspace_dir.clone());
        // Refresh the channels snapshot so /channels and /platforms reflect
        // any wizard-driven add/remove since launch.
        // Captured from the CURRENT config, before the swap below. The save
        // handler swaps `self.config` in as soon as the save completes (so the
        // next provisioner's clone sees this provisioner's writes), so reading
        // "previous" from `self.config` here read the already-swapped value:
        // previous always equalled new, and the restart never fired.
        let prev_channels_count = crate::channels::configured_channel_count(&self.config);
        self.context.provider_key_ok = provider_key_status(&config);
        self.context.approval_boundary = (
            config.channels_config.approval_owners.len(),
            config.channels_config.autonomous_tools,
        );
        self.context.channels_summary = crate::channels::channel_status_roster(&config)
            .into_iter()
            .map(|(name, configured, support, verification)| {
                (name.to_string(), configured, support, verification)
            })
            .collect();
        self.context.running_whatsapp_surface = config.channels_config.running_whatsapp_surface();
        let new_channels_count = crate::channels::configured_channel_count(&config);
        self.context.channels_autostart_count = new_channels_count;
        // When the configured channel set changes mid-session (e.g.
        // `/setup telegram`), restart the background channel runtime in
        // place so newly-added channels start polling — and removed ones
        // stop — without closing the TUI. The restart itself runs after
        // `self.config` is updated below (it reads `self.config`); surface a
        // short status line so the user knows it's taking effect live.
        // By content, not by count: a token rotation and a one-for-one channel
        // swap are both count-neutral. Suppressed when the save-complete branch
        // already restarted for this exact configuration — `reload_config` runs
        // on Esc right after, and restarting twice costs the 409 window twice.
        // One-shot: consumed here so a later reload still restarts.
        //
        // A policy change restarts the listeners too — they have to run under
        // the new policy — but it changed no channel setting, and the line
        // says so.
        let already_restarted = std::mem::take(&mut self.channels_restarted_for_save);
        let change = setup_change(&self.config, &config);
        let restart_needed = change != SetupChange::None && !already_restarted;
        if restart_needed {
            let msg = match change {
                SetupChange::Channels => match new_channels_count.cmp(&prev_channels_count) {
                    // Count-neutral: a token rotation, or a one-for-one swap. The
                    // old detector could not see this case at all.
                    std::cmp::Ordering::Equal => {
                        "✓ Channel settings changed — restarting listener(s) \
                         now. `/channels` shows the current state."
                            .to_string()
                    }
                    std::cmp::Ordering::Greater => format!(
                        "✓ {} new channel(s) configured — starting listener(s) now. \
                         `/channels` shows the current state.",
                        new_channels_count - prev_channels_count
                    ),
                    std::cmp::Ordering::Less => format!(
                        "✓ {} channel(s) removed — stopping their listener(s) now.",
                        prev_channels_count - new_channels_count
                    ),
                },
                _ => "✓ Approval policy applied — restarting listener(s) now so it takes effect."
                    .to_string(),
            };
            let _ = self.context.append_system_message(&msg);
            self.scrollback_queue.push(("system".to_string(), msg));
        }
        // `self.config` keeps the model the agent runs: a switch still waiting
        // for its own outcome has not been adopted here, so neither the
        // in-memory config nor the file names its model yet. Everything else
        // the file changed is adopted, and reaches the agent through the
        // reload below.
        self.config = config;
        // Config reload can flip `skills.entries.<name>.enabled` (CLI
        // `skills enable/disable`, an external edit, or the in-picker
        // toggle). Refresh the cached skill lists so `/skills`,
        // autocomplete, and the next agent turn reflect the new set —
        // model/providers/channels are already refreshed above; skills
        // were the missing surface.
        self.refresh_available_skills();
        if restart_needed {
            self.pending_setup_change = SetupChange::None;
            self.restart_channels();
        }
        // Push the new config to the agent actor so the next turn uses
        // the freshly-saved provider/api_key/model. Without this the
        // agent stays pinned to the launch-time config and reports
        // "Missing API key" even though the wizard saved one.
        let req_tx = self.context.req_tx.clone();
        tokio::spawn(async move {
            let _ = req_tx
                .send(crate::tui::TurnRequest::Reload {
                    id,
                    config: Box::new(reload_config),
                })
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
                    let status = tool_status_line(&output_preview, ok);
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
                        let hint = shell_block_hint(self.context.autonomy_preset);
                        let _ = self.context.append_system_message(&hint);
                        self.scrollback_queue.push(("system".into(), hint));
                    }
                }
            }
            AgentEvent::MemoryRecalled { keys } => {
                // `append_system_message` is the live path. `scrollback_queue`
                // and `commit_message_to_scrollback` still exist beside it but
                // nothing reads them any more — a muted `_tool_log` line pushed
                // there renders nowhere, which is how the first version of this
                // silently did nothing.
                let _ = self
                    .context
                    .append_system_message(&render_recalled_memories(&keys));
            }
            AgentEvent::Usage(u) => {
                // One event per turn. "This session" is the sum over the
                // turns, so each turn adds to the total rather than replacing
                // it; the first reported turn starts the total.
                let turn = TokenUsage {
                    prompt_tokens: u.input_tokens,
                    completion_tokens: u.output_tokens,
                    total_tokens: u.total_tokens,
                };
                self.context.token_usage = Some(match self.context.token_usage.take() {
                    Some(total) => TokenUsage {
                        prompt_tokens: total.prompt_tokens.saturating_add(turn.prompt_tokens),
                        completion_tokens: total
                            .completion_tokens
                            .saturating_add(turn.completion_tokens),
                        total_tokens: total.total_tokens.saturating_add(turn.total_tokens),
                    },
                    None => turn,
                });
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
            AgentEvent::ImageRefused { message } => {
                self.finalize_refusal(message);
            }
            AgentEvent::ReloadFailed { id, error } => {
                // The previous agent is still running and answers the next
                // prompt, so a rebuild that failed changes nothing: the label,
                // `self.config` and the file keep what they had. A failure is
                // the pending switch's verdict when it comes from the switch
                // itself or from any reload sent after it — those carry the
                // switch's model, so the switch did not land. A failure from an
                // older reload (`pending.id > id`) is not its verdict and
                // leaves the switch waiting.
                let was_a_model_switch = self
                    .pending_reload
                    .as_ref()
                    .is_some_and(|pending| pending.id <= id && pending.unsaved_config.is_some());
                if was_a_model_switch {
                    self.pending_reload = None;
                }
                let msg = if was_a_model_switch {
                    format!("Model not changed: the agent could not be rebuilt: {error}")
                } else {
                    format!("Config reload failed, keeping the running config: {error}")
                };
                self.context.last_error = Some(msg.clone());
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
            }
            AgentEvent::ReloadComplete {
                id,
                mcp_servers_configured,
                mcp_tools_by_server,
                security,
            } => {
                // The reload rebuilt the agent, so the actor can run a turn
                // again: from here on a turn it answers with an error is
                // followed by that turn's own `Done` (`finalize_error`).
                self.actor_has_agent = true;
                // Refresh the TUI's cached MCP snapshot so `/mcp`
                // reflects the post-reload state. The agent already
                // has the new tools and can use them on the next
                // turn — this just keeps the UI's view in sync.
                self.context.mcp_servers_configured = mcp_servers_configured.into_iter().collect();
                self.context.mcp_tools_by_server = mcp_tools_by_server;
                // An agent is now running the config THIS reload carried, so
                // the status bar may name its model and a `/model` switch may
                // write its config. Reload ids are monotonic and every reload
                // sent after a pending switch carries its model, so an outcome
                // whose id is at or past the pending one IS that switch's
                // verdict; an older outcome (`pending.id > id`) moves nothing.
                if self
                    .pending_reload
                    .as_ref()
                    .is_some_and(|pending| pending.id <= id)
                {
                    // Read the switch's model off the slot BEFORE taking it:
                    // what is written below is the config this app holds now
                    // with that model on it, not the snapshot the switch sent.
                    let adopted = self.with_pending_model(self.config.clone());
                    let pending = self
                        .pending_reload
                        .take()
                        .expect("the pending reload was just matched");
                    if let Some(label) = pending.model_label {
                        self.context.model = label;
                    }
                    if pending.unsaved_config.is_some() {
                        // An agent is running that model now, so `self.config`
                        // may name it and the file follows on the next task.
                        //
                        // The snapshot the switch sent is read for its model
                        // only. Swapping it back in would revert everything
                        // that landed while the switch was in flight — a skill
                        // toggle, an `/autonomy` preset, a file edit the
                        // watcher picked up — in memory and on disk, because
                        // this save is the last writer.
                        self.config = adopted.clone();
                        let label = self.context.model.clone();
                        self.persist_model_switch(adopted, label);
                        let msg = format!("Model set to: {}", self.context.model);
                        self.context.last_error = None;
                        let _ = self.context.append_system_message(&msg);
                        self.scrollback_queue.push(("system".to_string(), msg));
                    }
                }
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
                // History replaced — selection anchors (msg_idx, line_idx)
                // now point at different text; drop them.
                self.context.selection = None;
                self.selection_drag_anchor = None;
                self.selection_dragged = false;

                // Persist the new shape so it survives restart (only when a
                // session is bound — compaction always runs on an active one).
                // Seam invariant: an in-memory `Message.id` equals its stored
                // row id. `replace_messages` re-numbers every row, so the ids
                // it returns are copied back here; a stale id would match
                // every stored row in a later `/undo` or `/retry` cut.
                if let Some(sid) = self.context.session_id.clone() {
                    match self
                        .context
                        .session_store
                        .replace_messages(&sid, &self.context.messages)
                    {
                        Ok(ids) => {
                            for (msg, id) in self.context.messages.iter_mut().zip(ids) {
                                msg.id = id;
                            }
                        }
                        Err(e) => tracing::warn!("failed to persist compacted history: {e}"),
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
            // overlay, never the web-modal backend, so it never receives these
            // events. Covered for exhaustiveness only.
            AgentEvent::ApprovalRequest { .. } | AgentEvent::ApprovalResolved { .. } => {}
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

        // This `Done` ends the turn at the front of the queue. A refused turn
        // already showed its one line (see `finalize_refusal`) and its text
        // is back in the input box; nothing ran, so it carries no reply, and
        // committing an empty assistant row plus a "[no response …]" line
        // would be a reply the user never asked for. Whether this turn was
        // refused is its own fact — a message submitted since says nothing
        // about it.
        let refused = self
            .pending_turns
            .pop_front()
            .is_some_and(|turn| turn.refused);
        if refused {
            self.end_turn_or_run_the_next();
            return;
        }

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
        // A `[no response from model` line is the TUI's own error text, not a
        // reply; it is stored as a note so the model never receives it.
        let persisted = if body.starts_with("[no response from model") {
            self.context.append_note_message(&body)
        } else {
            self.context
                .append_assistant_message_with_tools(&body, tool_calls_json)
        };
        let persist_ok = match persisted {
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

        self.end_turn_or_run_the_next();

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

    /// End the turn that just finished: the next queued turn starts streaming,
    /// or the app goes back to `Ready` when none is waiting.
    fn end_turn_or_run_the_next(&mut self) {
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
    }

    /// Finalize a turn on [`AgentEvent::ImageRefused`] — the agent would not
    /// send the message, because one of its images could not be prepared, and
    /// nothing entered its history.
    ///
    /// So the TUI takes the message back rather than showing a failed turn:
    /// the row leaves the screen and the store (so `/undo`, `/retry` and a
    /// later resume cannot bring it back), its text returns to the input box
    /// to be edited and sent again, and the refusal is shown once, as one
    /// line. A turn that fails after the message was sent keeps today's
    /// behavior and does not come here.
    fn finalize_refusal(&mut self, message: String) {
        self.shell_blocks_this_turn = 0;
        self.autonomy_hint_shown_this_turn = false;
        // The refusal belongs to the turn in flight — the front of the queue.
        // Marking it here and taking back the row IT was submitted for is
        // what keeps a message submitted in the meantime from being taken
        // back in its place, or from forgetting that this turn never ran.
        let refused_row = self.pending_turns.front_mut().map(|turn| {
            turn.refused = true;
            turn.row_id
        });
        if let Some(text) = refused_row
            .flatten()
            .and_then(|row_id| self.context.take_back_refused_row(row_id))
        {
            self.context.input_buffer = text;
            self.context.cursor_to_end();
        }
        // Stored as a note, like a failed turn's error line: the text is back
        // in the input box, and a note is never replayed to the model.
        if let Err(e) = self.context.append_note_message(&message) {
            self.context.last_error = Some(format!("failed to persist refusal: {e}"));
        }
        self.scrollback_queue.push(("system".to_string(), message));
        // Nothing ran, so the previous turn's `last_error` is neither
        // confirmed nor cleared here — the same as a cancelled turn.
    }

    /// Finalize a turn on `AgentEvent::Error`. Surfaces the error as a
    /// visible assistant message (so it shows up in chat history) AND sets
    /// `last_error` so the status bar reflects it until cleared.
    ///
    /// The turn itself ends with the `Done` that follows the error — except
    /// when the actor holds no agent, which answers a `Submit` with this
    /// error alone. Then this call retires the queued turn, so every submit
    /// still has exactly one pop.
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
        if let Err(e) = self.context.append_note_message(&chat_body) {
            self.context.last_error = Some(format!("failed to persist error: {e}"));
        } else {
            self.context.last_error = Some(status_line);
        }
        // Commit error message to scrollback so the user sees it inline.
        self.scrollback_queue
            .push(("system".to_string(), chat_body));
        // With no agent, this error is the whole answer to the turn that was
        // queued for it: the actor could not run it and sends no `Done`, so
        // the queue entry is retired here — one `Submit`, one pop. Left at the
        // front it would be the row the next refusal takes back, the same way
        // an unanswered turn would swallow the `Done` of the turn after it.
        // A running agent's error is always followed by that turn's own
        // `Done`, which pops it; popping here as well would pop twice.
        if !self.actor_has_agent {
            self.pending_turns.pop_front();
        }
        self.state = AppState::Ready;
    }

    /// Handle a slash command (text after the leading `/`).
    pub async fn handle_command(&mut self, cmd: &str) -> Result<()> {
        let dispatched = self.command_registry.dispatch(cmd, &mut self.context);
        // A reseed a command queued must reach the agent before any request
        // the command dispatches, and before the next prompt.
        self.context.flush_pending_reseed().await;
        match dispatched? {
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
            CmdResult::SensitiveMessage { display, persisted } => {
                // The redacted form goes to the session store; the real one
                // only ever reaches the terminal.
                let _ = self.context.append_system_message(&persisted);
                self.scrollback_queue.push(("system".to_string(), display));
            }
            CmdResult::Overlay(content) => {
                // Inline-mode: render the overlay's body straight into
                // terminal scrollback (Hermes / Claude Code feel) so the
                // user can scroll back through long output natively. We
                // flatten the active tab plus a header line.
                let mut out = String::new();
                use std::fmt::Write as _;
                let _ = writeln!(out, "{}", content.title);
                for (i, tab) in content.tabs.iter().enumerate() {
                    if content.tabs.len() > 1 {
                        let marker = if i == content.active_tab {
                            "▸ "
                        } else {
                            "  "
                        };
                        let _ = writeln!(out, "\n{}{}", marker, tab.label);
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
                self.dispatch_resubmit(text, None).await;
            }
            CmdResult::RetryLast(text) => {
                // A retry refused mid-stream must change nothing, so the old
                // reply is dropped only once the resubmit can go out.
                if matches!(self.state, AppState::Ready) {
                    if let Err(e) = self.context.drop_last_reply() {
                        self.context.last_error = Some(format!("retry failed: {e}"));
                        return Ok(());
                    }
                }
                // The retried message keeps its row on the screen; that row is
                // the one a refusal of this turn takes back.
                let row_id = self
                    .context
                    .messages
                    .iter()
                    .rposition(|m| m.role == "user")
                    .map(|at| self.context.messages[at].id);
                self.dispatch_resubmit(text, row_id).await;
            }
            CmdResult::SetModel(target) => {
                // The typed `/model <provider:model>` form: split once here,
                // then hand the switch two values.
                let (provider, model) = super::commands::split_model_target(&target);
                self.apply_model_selection(provider, model);
            }
            CmdResult::OpenListPicker(picker) => {
                // Any other picker replaces the model picker's side table.
                self.model_picker_entries = None;
                self.list_picker = Some(picker);
            }
            CmdResult::OpenModelPicker { picker, entries } => {
                // The rows' keys are opaque (`model-row:{n}`); the provider and
                // model each row stands for travel beside the picker.
                self.model_picker_entries = Some(entries);
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
            CmdResult::Compact { keep_last } => {
                // A history reseed held for the agent goes first, so the
                // compaction runs on the session the screen shows.
                self.context.flush_pending_reseed().await;
                match self
                    .context
                    .req_tx
                    .send(TurnRequest::Compact { keep_last })
                    .await
                {
                    Ok(()) => {
                        let msg = format!(
                            "Compacting older turns (preserving last {keep_last} verbatim). \
                             Summary streams below…"
                        );
                        let _ = self.context.append_system_message(&msg);
                        self.scrollback_queue.push(("system".to_string(), msg));
                    }
                    Err(e) => {
                        self.context.last_error = Some(format!(
                            "Failed to dispatch /compress: {e}. Is the agent loop running?"
                        ));
                    }
                }
            }
            CmdResult::CancelTurnWithMessage(msg) => {
                // `/deny` resolved a pending approval as Deny. Mirror the
                // inline N/Esc path (resolve_pending_approval): show the
                // note, then cancel the turn so the LLM doesn't react to
                // the tool error by trying alternative commands.
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
                if let AppState::Streaming { cancelling, .. } = &mut self.state {
                    *cancelling = true;
                }
                if let Err(e) = self.context.req_tx.send(TurnRequest::Cancel).await {
                    self.context.last_error = Some(format!("cancel failed: {e}"));
                }
            }
            CmdResult::OpenFirstRunWizard => {
                self.first_run_wizard = Some(super::FirstRunWizard::new(self.profile.clone()));
            }
            CmdResult::OpenClawhubInstallPicker { initial_query } => {
                self.open_clawhub_install_picker(initial_query);
            }
            CmdResult::OpenSkillInEditor {
                slug,
                path,
                initial,
                is_new,
            } => {
                // Suspending the terminal mid-stream would tear down a screen
                // the agent is still writing to, and hand the editor a terminal
                // the agent keeps printing into. Refuse rather than queue or
                // cancel — the user can retry in a moment.
                if matches!(self.state, AppState::Streaming { .. }) {
                    let msg = "Wait for the current response to finish, then try again.";
                    let _ = self.context.append_system_message(msg);
                    self.scrollback_queue
                        .push(("system".to_string(), msg.to_string()));
                } else {
                    self.skill_editor_request = Some(SkillEditRequest {
                        slug,
                        path,
                        initial,
                        is_new,
                    });
                }
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
                // The chosen row names its row, not a model: the pair comes
                // from the entries the picker was opened with, never from
                // splitting a joined string back apart. One picker is open at a
                // time, so the table is spent here.
                let entries = self.model_picker_entries.take();
                let entry = key
                    .strip_prefix(super::commands::ROW_KEY_PREFIX)
                    .and_then(|index| index.parse::<usize>().ok())
                    .and_then(|index| entries.as_ref()?.get(index));
                match entry.map(|e| (e.provider.clone(), e.model_id.clone())) {
                    Some((provider, model)) => self.apply_model_selection(Some(&provider), &model),
                    None => {
                        let msg = "That model row is no longer available — reopen /model.";
                        self.context.last_error = Some(msg.to_string());
                        let _ = self.context.append_system_message(msg);
                        self.scrollback_queue
                            .push(("system".to_string(), msg.to_string()));
                    }
                }
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
                // The label is not written here: the agent is not rebuilt on a
                // pick, so the model that answers the next message is the one
                // already running. The picked session's own model is named in
                // one note after the replay instead.

                self.context.messages.clear();
                // Another session's total is not this session's: the store
                // keeps no per-turn usage, so the picked one starts unreported.
                self.context.token_usage = None;
                // A title set by hand belongs to the session it was set on,
                // like `/new`: the picked session titles itself from its own
                // first user row.
                self.context.title_set_by_hand = false;
                // History replaced (resume) — selection anchors are stale.
                self.context.selection = None;
                self.selection_drag_anchor = None;
                self.selection_dragged = false;
                if let Err(e) = self.context.load_session_messages() {
                    self.context.last_error = Some(format!("load_session failed: {e}"));
                    return;
                }
                // The agent takes the picked session's turns in place of the
                // ones it held, so the next prompt is that session's.
                self.context
                    .reseed_agent(&crate::sessions::turns_for_history(&self.context.messages));
                self.context.flush_pending_reseed().await;
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
                // A session opened under another model says so, once, after the
                // replay — before it the note would be replayed a second time.
                if session_model_differs(&session.model, &self.context.model) {
                    let msg = format!(
                        "This session was created with {model}; the agent still runs {running}. \
                         Use /model to switch.",
                        model = session.model.trim(),
                        running = self.context.model,
                    );
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".to_string(), msg));
                }
            }
            ListPickerKind::Personality => {
                let msg = match crate::persona::PresetId::from_slug(&key) {
                    Some(preset) => match crate::profile::ProfileManager::active().and_then(|p| {
                        crate::persona::apply_update(
                            &p,
                            crate::persona::PersonaUpdate {
                                preset: Some(preset),
                                ..Default::default()
                            },
                        )
                    }) {
                        Ok(_) => format!(
                            "Personality preset set to {}.\nTakes effect on your next new conversation (/new) or reload.",
                            preset.slug()
                        ),
                        Err(e) => format!("Failed to set personality: {e}"),
                    },
                    None => format!("Unknown preset '{key}'."),
                };
                let _ = self.context.append_system_message(&msg);
                self.scrollback_queue.push(("system".to_string(), msg));
            }
            ListPickerKind::Skill => {
                // A gated/disabled row (non-empty reasons in
                // `available_skills_with_status`) isn't in the agent's
                // context — `load_skills_with_config` excluded it — so
                // prefilling `Use the X skill: ` would send a request the
                // agent can't fulfil. Show the gating reason instead.
                let gated = self
                    .context
                    .available_skills_with_status
                    .iter()
                    .find(|(s, _)| s.name == key)
                    .map(|(_, reasons)| reasons.clone())
                    .filter(|r| !r.is_empty());
                if let Some(reasons) = gated {
                    let msg = format!(
                        "'{key}' is not active — {}. Enable it with `rantaiclaw skills enable {key}` \
                         (or Ctrl+I to install missing deps), then reload.",
                        reasons.join("; ")
                    );
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".to_string(), msg));
                } else {
                    // Pre-fill an invocation prompt into the input buffer.
                    // The user can edit, append context, and Enter to send.
                    self.context.input_buffer = format!("Use the {key} skill: ");
                    self.context.cursor_to_end();
                    self.refresh_autocomplete();
                }
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
            ListPickerKind::ClawhubInstall | ListPickerKind::ClawhubPublisher => {
                // Enter for both is handled inline in `handle_key` — the
                // browser splits it (Focus::Search → search, Focus::List →
                // install) and the publisher prompt always installs — so
                // neither reaches dispatch. Defensive no-op.
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
                    // The live gate did NOT change — do not flip the status-bar
                    // preset or print a success line (that would assert a level
                    // not in force). Mirror the Shift+Tab handler, which returns
                    // early on the same failure.
                    let msg = format!("⚠ Preset written, but live reload failed: {e}");
                    let _ = self.context.append_system_message(&msg);
                    self.scrollback_queue.push(("system".into(), msg));
                    return;
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
            ListPickerKind::Cron => match Self::load_config_for_cron() {
                Ok(config) => match Self::build_cron_detail_panel(&config, &key) {
                    Some(panel) => self.info_panel = Some(panel),
                    None => self.cron_system_msg(&format!("Cron job {key} not found.")),
                },
                Err(e) => self.cron_system_msg(&format!("Could not load config: {e}")),
            },
        }
    }

    /// Load the running config for `/cron` store calls (brief file read via
    /// `block_in_place`). Associated fn so it can run while `self.info_panel` is
    /// being reassigned without a borrow conflict.
    fn load_config_for_cron() -> anyhow::Result<crate::config::Config> {
        let h = tokio::runtime::Handle::try_current()?;
        tokio::task::block_in_place(|| {
            h.block_on(async { crate::config::Config::load_or_init().await })
        })
    }

    /// Append a `/cron` system message to the chat + scrollback.
    fn cron_system_msg(&mut self, msg: &str) {
        let _ = self.context.append_system_message(msg);
        self.scrollback_queue
            .push(("system".into(), msg.to_string()));
    }

    /// Build the cron-job detail panel (used by the picker dispatch + the
    /// pause action to rebuild after a state change). Associated fn (no `self`)
    /// so it can be called while `self.info_panel` is reassigned.
    fn build_cron_detail_panel(
        config: &crate::config::Config,
        job_id: &str,
    ) -> Option<super::widgets::InfoPanel> {
        use super::widgets::{InfoPanel, InfoSection, StatusKind};
        let job = crate::cron::get_job(config, job_id).ok()?;
        let mut detail = InfoSection::new("Job")
            .status_with(
                if job.enabled {
                    StatusKind::Ok
                } else {
                    StatusKind::Warn
                },
                "State",
                if job.enabled { "enabled" } else { "paused" },
            )
            .status_with(StatusKind::Info, "Schedule", job.schedule.to_string())
            .status_with(StatusKind::Info, "Next run", job.next_run.to_rfc3339())
            .status_with(
                StatusKind::Info,
                "Last run",
                job.last_run.map_or("never".to_string(), |d| d.to_rfc3339()),
            )
            .status_with(
                match job.last_status.as_deref() {
                    Some("ok") => StatusKind::Ok,
                    Some("error" | "refused") => StatusKind::Fail,
                    _ => StatusKind::Info,
                },
                "Last status",
                job.last_status.clone().unwrap_or_else(|| "n/a".into()),
            );
        let what = if job.command.is_empty() {
            job.prompt.clone().unwrap_or_default()
        } else {
            job.command.clone()
        };
        detail = detail.status_with(
            StatusKind::Info,
            if job.command.is_empty() {
                "Prompt"
            } else {
                "Command"
            },
            what,
        );

        let mut history = InfoSection::new("Recent runs");
        match crate::cron::list_runs(config, job_id, 5) {
            Ok(runs) if runs.is_empty() => {
                history = history.status_with(StatusKind::Info, "—", "no runs yet");
            }
            Ok(runs) => {
                for r in runs {
                    history = history.status_with(
                        if r.status == "ok" {
                            StatusKind::Ok
                        } else {
                            StatusKind::Fail
                        },
                        r.started_at.to_rfc3339(),
                        format!("{} ({}ms)", r.status, r.duration_ms.unwrap_or(0)),
                    );
                }
            }
            Err(e) => history = history.status_with(StatusKind::Fail, "history", e.to_string()),
        }
        Some(
            InfoPanel::new("Cron Job")
                .with_subtitle(crate::cron::cron_display_id(&job))
                .with_footer("[r] run · [p] pause/resume · [d] delete · Esc close")
                .with_cron_actions(job_id)
                .section(detail)
                .section(history),
        )
    }

    /// Handle a cron detail-panel action-key (run / pause-resume / delete).
    ///
    /// The wrapper loads the config from disk; the work is in
    /// [`Self::cron_panel_action_with_config`], so the gate can be tested
    /// without a config file.
    fn cron_panel_action(&mut self, action: char) {
        // No cron panel, no action: nothing is loaded and nothing is printed.
        if self
            .info_panel
            .as_ref()
            .and_then(|p| p.cron_job_id.as_ref())
            .is_none()
        {
            return;
        }
        let config = match Self::load_config_for_cron() {
            Ok(c) => c,
            Err(e) => {
                self.cron_system_msg(&format!("Could not load config: {e}"));
                return;
            }
        };
        self.cron_panel_action_with_config(action, &config);
    }

    /// The action-key handler, with the config handed in.
    ///
    /// Every one of these keys writes (a run records history, a pause/resume
    /// edits the job, a delete removes it), so the cron feature switch closes
    /// them — with the same words every other door uses.
    fn cron_panel_action_with_config(&mut self, action: char, config: &crate::config::Config) {
        let Some(id) = self.info_panel.as_ref().and_then(|p| p.cron_job_id.clone()) else {
            return;
        };
        if !config.cron.enabled {
            self.cron_system_msg(&format!("✗ {}", crate::cron::CRON_DISABLED_MESSAGE));
            return;
        }
        match action {
            'r' => {
                // Run DETACHED — a shell job (up to 120s) or agent job (up to
                // 600s) must NOT block the render loop. `run_job_manual` records
                // to run history; the user reopens the job to see the result.
                let job = match crate::cron::get_job(config, &id) {
                    Ok(j) => j,
                    Err(e) => {
                        self.cron_system_msg(&format!("✗ {e}"));
                        return;
                    }
                };
                let cfg = config.clone();
                let security =
                    crate::security::SecurityPolicy::from_config(&cfg.autonomy, &cfg.workspace_dir);
                tokio::spawn(async move {
                    let _ =
                        crate::cron::scheduler::run_job_manual(&cfg, &security, &job, None, None)
                            .await;
                });
                self.cron_system_msg(&format!(
                    "▶ Started run of cron job {id} — reopen the job for the result."
                ));
            }
            'p' => {
                let enabled = crate::cron::get_job(config, &id).is_ok_and(|j| j.enabled);
                let r = if enabled {
                    crate::cron::pause_job(config, &id)
                } else {
                    crate::cron::resume_job(config, &id)
                };
                match r {
                    Ok(_) => self.info_panel = Self::build_cron_detail_panel(config, &id),
                    Err(e) => self.cron_system_msg(&format!("✗ {e}")),
                }
            }
            'd' => match crate::cron::remove_job(config, &id) {
                Ok(()) => {
                    self.info_panel = None;
                    self.cron_system_msg(&format!("🗑 Removed cron job {id}"));
                }
                Err(e) => self.cron_system_msg(&format!("✗ {e}")),
            },
            _ => {}
        }
    }

    /// Toggle the highlighted `/skills` picker row's enabled/disabled state
    /// (Ctrl+E). Persists via `skills::set_skill_enabled` — the same writer
    /// `rantaiclaw skills enable`/`disable` uses (plan 037) — so both
    /// surfaces stay in sync. Refreshes the cached skill lists and rebuilds
    /// the picker items in place so the `✗` glyph flips live, then pushes
    /// the updated config to the agent actor so the running turn picks up
    /// the change without a relaunch.
    async fn toggle_selected_skill(&mut self) {
        let Some(key) = self
            .list_picker
            .as_ref()
            .and_then(|p| p.current().map(|item| item.key.clone()))
        else {
            return;
        };
        let currently_enabled = !self
            .context
            .available_skills_with_status
            .iter()
            .find(|(s, _)| s.name == key)
            .is_some_and(|(_, reasons)| reasons.iter().any(|r| r == "disabled in config.toml"));
        let (updated, canonical) =
            match crate::skills::set_skill_enabled(&self.config, &key, !currently_enabled) {
                Ok(v) => v,
                Err(e) => {
                    self.context.last_error = Some(format!("skill toggle failed: {e}"));
                    return;
                }
            };
        if let Err(e) = updated.save().await {
            self.context.last_error = Some(format!("failed to save config: {e}"));
            return;
        }
        self.config = updated;
        self.refresh_available_skills();
        let items = self.skill_picker_items();
        // Rebuild (rather than `set_items`, which resets the cursor to the
        // top) so the picker reopens with the just-toggled row still
        // highlighted — mirrors the install-tick rebuild at
        // `tick_clawhub_install`.
        if let Some(p) = self.list_picker.as_mut() {
            if p.kind == crate::tui::widgets::ListPickerKind::Skill {
                let title = p.title.clone();
                let empty_hint = p.empty_hint.clone();
                *p = crate::tui::widgets::ListPicker::new(
                    crate::tui::widgets::ListPickerKind::Skill,
                    title,
                    items,
                    Some(&canonical),
                    empty_hint,
                );
            }
        }
        let new_state = if currently_enabled {
            "disabled"
        } else {
            "enabled"
        };
        let msg = format!("✓ {canonical} {new_state}. Reloading agent…");
        let _ = self.context.append_system_message(&msg);
        self.scrollback_queue.push(("system".to_string(), msg));
        let id = self.next_reload_id();
        let req_tx = self.context.req_tx.clone();
        let config = self.with_pending_model(self.config.clone());
        tokio::spawn(async move {
            let _ = req_tx
                .send(crate::tui::TurnRequest::Reload {
                    id,
                    config: Box::new(config),
                })
                .await;
        });
    }

    /// Open the ClawHub install picker. Empty query → top-by-stars listing.
    /// Search fires only on Enter while focused on the search bar (per
    /// tester request — keystroke-fire churned the network too aggressively).
    fn open_clawhub_install_picker(&mut self, initial_query: Option<String>) {
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
        // A ClawHub round trip runs six to sixteen seconds here. Without this
        // the picker kept showing the previous results the whole time, with
        // nothing to say a search was running — the user could not tell
        // whether Enter had registered.
        self.clawhub_search_frame = Some(0);
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
            should_reload = watcher.try_changed();
        }
        if !should_reload {
            return;
        }
        // A watcher event fires on ANY write to the file, this app's own
        // included: a `/model` switch writes `config.toml` once the rebuild it
        // asked for has succeeded, and a rewrite that changes no content fires
        // the same way. Comparing the bytes is what stops that tick from
        // pushing a second `Reload` of the config the switch just rebuilt from
        // — the same fingerprint gate the gateway's reloader uses.
        let on_disk = crate::config::fingerprint::fingerprint_file(&self.profile.config_toml());
        if *self
            .config_fingerprint
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            == on_disk
        {
            tracing::info!("config.toml tick carries bytes this app already has — not reloading");
            return;
        }
        *self
            .config_fingerprint
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = on_disk;
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
            let id = self.next_reload_id();
            let config = self.with_pending_model(self.config.clone());
            let req_tx = self.context.req_tx.clone();
            tokio::spawn(async move {
                let _ = req_tx
                    .send(crate::tui::TurnRequest::Reload {
                        id,
                        config: Box::new(config),
                    })
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

    /// Animate the in-flight search indicator. Same Braille spinner and the
    /// same per-frame cadence as the install ticker below, so the two read as
    /// one language rather than two.
    fn tick_clawhub_search(&mut self) {
        use super::widgets::{ListPickerKind, PickerStatus, StatusTone};
        const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

        let Some(frame) = self.clawhub_search_frame.as_mut() else {
            return;
        };
        *frame = (*frame + 1) % SPINNER.len();
        let glyph = SPINNER[*frame];
        if let Some(p) = self.list_picker.as_mut() {
            if p.kind == ListPickerKind::ClawhubInstall {
                p.status = Some(PickerStatus {
                    text: format!("{glyph} searching ClawHub…"),
                    tone: StatusTone::Busy,
                });
            }
        }
    }

    /// Drain any pending ClawHub search results and apply the latest one
    /// matching the current search version. Called on each event loop
    /// tick (after a key is processed) so results land before the next
    /// render. Stale results (older versions) are silently discarded.
    fn drain_clawhub_search_results(&mut self) {
        use super::widgets::{ListPickerItem, ListPickerKind, PickerStatus, StatusTone};

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

        // Read before the picker borrow — the rows need to know what is
        // already on disk, and `self` is not reachable once `picker` is held.
        let installed = self.installed_slug_owners();

        let Some(picker) = self.list_picker.as_mut() else {
            return;
        };
        if picker.kind != ListPickerKind::ClawhubInstall {
            return;
        }

        self.clawhub_search_frame = None;
        match result {
            Ok(skills) => {
                picker.status = None;
                let items: Vec<ListPickerItem> = skills
                    .iter()
                    .map(|s| clawhub_result_item(s, &installed))
                    .collect();
                if items.is_empty() {
                    picker.empty_hint = if picker.query.trim().is_empty() {
                        "ClawHub returned nothing.".to_string()
                    } else {
                        format!("No ClawHub skill matches '{}'.", picker.query.trim())
                    };
                }
                picker.set_items(items);
            }
            Err(e) => {
                // Logging alone left the previous results on screen, which is
                // indistinguishable from a search that succeeded and returned
                // the same thing. A failure the user cannot see is a failure
                // they will act on.
                tracing::warn!("ClawHub search failed: {e:#}");
                picker.status = Some(PickerStatus {
                    text: format!("✗ search failed — {}", root_cause_of(&e)),
                    tone: StatusTone::Error,
                });
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
        let mut completion: Option<Result<String, ClawhubInstallFailure>> = None;
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
            Some(Err(ClawhubInstallFailure::Ambiguous(ambiguous))) => {
                // Not a failure the user can only read about: the slug is
                // shared by several publishers, so replace the rows with the
                // candidates and let them say which one they meant. Keys are
                // fully qualified, so the next Enter installs directly.
                self.clawhub_install_in_progress = None;
                self.clawhub_install_completion_rx = None;
                self.clawhub_install_completion_tx = None;
                let slug = ambiguous.slug.clone();
                let items = clawhub_candidate_items(&ambiguous);
                // A fresh picker of its own kind, not the browser with its
                // rows swapped. Reusing the browser meant its search keys
                // stayed live: one keystroke returned focus to the search box
                // and the next Enter replaced these candidates with search
                // results, under a title still asking which publisher.
                // Building anew also drops the stale query the browser left
                // behind, which that search would have run.
                self.list_picker = Some(crate::tui::widgets::ListPicker::new(
                    crate::tui::widgets::ListPickerKind::ClawhubPublisher,
                    format!("`{slug}` — {} publishers, pick one", items.len()),
                    items,
                    None,
                    "No publishers returned.",
                ));
                if let Some(p) = self.list_picker.as_mut() {
                    p.focus = crate::tui::widgets::list_picker::Focus::List;
                }
            }
            Some(Err(ClawhubInstallFailure::Other(error_msg))) => {
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
    }

    fn close_skill_deps_install_state(&mut self) {
        self.skill_deps_install_in_progress = None;
        self.skill_deps_install_completion_rx = None;
        self.skill_deps_install_completion_tx = None;
    }

    fn open_setup_overlay(&mut self, name: String) -> anyhow::Result<()> {
        use crate::onboard::provision::provisioner_for_with_force;

        // The session's `--force` intent travels with the provisioner, so a
        // surface that can only act on an answer (the approvals picker)
        // knows whether it may replace what is already on disk.
        let prov = provisioner_for_with_force(&name, self.setup_force)
            .ok_or_else(|| anyhow::anyhow!("unknown provisioner: {name}"))?;

        if prov.category() == crate::onboard::provision::ProvisionerCategory::Channel {
            let catalog_key = crate::channels::catalog_key_for_provisioner(&name);
            if !crate::channels::channel_is_usable(catalog_key) {
                anyhow::bail!("\"{name}\" is under development and cannot be set up yet");
            }
        }

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
        let prov_category = prov.category();
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
                // A provisioner that stopped early has configured nothing.
                // Installing the core skill or saving here would produce the
                // false "channel is set up" signal that the skill-install
                // ordering exists to prevent. `save_tx` drops unsent, so the
                // wizard's advance gate sees `Closed` and holds the overlay
                // open on the `Failed` event the provisioner already emitted.
                Ok(crate::onboard::provision::ProvisionOutcome::Aborted(reason)) => {
                    tracing::info!(
                        provisioner = prov_name,
                        "provisioner aborted, nothing persisted: {reason}"
                    );
                }
                Ok(crate::onboard::provision::ProvisionOutcome::Configured) => {
                    // A configured multi-user channel is the point at which
                    // the owner needs to manage permissions from chat, so make
                    // sure the skill that explains those tools is present.
                    // Runs before the config save below because it touches the
                    // profile's skills dir, not the config.
                    let finalize = crate::onboard::provision::finalize_channel(
                        prov_category,
                        &profile,
                        &config,
                    );
                    if let Some(guidance) = finalize.owner_guidance {
                        let _ = save_failure_tx
                            .send(crate::onboard::provision::ProvisionEvent::Message {
                                severity: crate::onboard::provision::Severity::Warn,
                                text: guidance,
                            })
                            .await;
                    }

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

                            // Reload the managed daemon so a channel configured
                            // here is actually live — only AFTER a successful
                            // save (the daemon re-reads config.toml). Blocking
                            // (shells out), so run it off the worker; best-effort
                            // — no managed daemon means nothing to reload.
                            if finalize.needs_daemon_reload {
                                match tokio::task::spawn_blocking(
                                    crate::channels::reload_managed_daemon,
                                )
                                .await
                                {
                                    Ok(Ok(true)) => {
                                        let _ = save_failure_tx
                                            .send(crate::onboard::provision::ProvisionEvent::Message {
                                                severity: crate::onboard::provision::Severity::Info,
                                                text: "Reloaded the running daemon to apply the new channel."
                                                    .into(),
                                            })
                                            .await;
                                    }
                                    Ok(Ok(false)) => {} // no managed daemon — nothing to reload
                                    Ok(Err(e)) => {
                                        let _ = save_failure_tx
                                            .send(crate::onboard::provision::ProvisionEvent::Message {
                                                severity: crate::onboard::provision::Severity::Warn,
                                                text: format!(
                                                    "Saved, but couldn't reload the daemon automatically: {e}"
                                                ),
                                            })
                                            .await;
                                    }
                                    Err(e) => {
                                        let _ = save_failure_tx
                                            .send(crate::onboard::provision::ProvisionEvent::Message {
                                                severity: crate::onboard::provision::Severity::Warn,
                                                text: format!("Daemon reload task panicked: {e}"),
                                            })
                                            .await;
                                    }
                                }
                            }
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
    /// without appending it to history. `row_id` is the store id of the user
    /// row this turn answers, or `None` for a resubmit that wrote no row
    /// (`/continue`) — there is nothing for a refusal to take back then.
    /// Refuses while a turn is already streaming — the user should cancel
    /// first.
    async fn dispatch_resubmit(&mut self, text: String, row_id: Option<i64>) {
        match self.state {
            AppState::Streaming { .. } => {
                self.context.last_error = Some(
                    "Cannot retry while a response is streaming. Press Ctrl+C to cancel first."
                        .to_string(),
                );
            }
            AppState::Ready => {
                self.context.flush_pending_reseed().await;
                self.pending_turns.push_back(SubmittedTurn {
                    row_id,
                    refused: false,
                });
                if let Err(e) = self
                    .context
                    .req_tx
                    .send(TurnRequest::Submit {
                        text,
                        conversation_id: self.context.conversation_scope(),
                    })
                    .await
                {
                    // Nothing will ever answer this turn, so it must not sit
                    // in the queue and be mistaken for a later one.
                    self.pending_turns.pop_back();
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
    /// `integration` / `runtime` / `routing`.
    fn open_category_sub_picker(&mut self, cat_key: &str) {
        use crate::onboard::provision::{available, provisioner_for, ProvisionerCategory};
        use crate::tui::commands::setup::{cat_label, category_from_key, channel_picker_entries};
        use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

        let Some(category) = category_from_key(cat_key) else {
            let msg = format!("Unknown setup category: {cat_key}");
            let _ = self.context.append_system_message(&msg);
            self.scrollback_queue.push(("system".into(), msg));
            return;
        };

        let title = format!("{} setup", cat_label(category));
        let empty_hint = format!(
            "no {} provisioners available",
            cat_label(category).to_lowercase()
        );

        let picker = if category == ProvisionerCategory::Channel {
            ListPicker::with_entries(
                ListPickerKind::SetupChannel,
                title,
                channel_picker_entries(),
                None,
                empty_hint,
            )
        } else {
            let items: Vec<ListPickerItem> = available()
                .into_iter()
                .filter_map(|(name, desc)| {
                    let p = provisioner_for(name)?;
                    if p.category() == category {
                        Some(ListPickerItem {
                            key: name.to_string(),
                            primary: name.to_string(),
                            secondary: desc.to_string(),
                            disabled: false,
                        })
                    } else {
                        None
                    }
                })
                .collect();
            ListPicker::new(
                ListPickerKind::SetupChannel, // re-used as the generic "category sub-picker" kind
                title,
                items,
                None,
                empty_hint,
            )
        };
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

        let _ = stream_committed_chars; // kept on App for future re-introduction
        let pending_approval_snapshot = self.pending_approval.clone();
        terminal.draw(|frame| {
            let area = frame.area();

            // Full-screen modals own the entire screen. Priority order matches
            // the old alt-screen path: login gate > setup overlay > first-run
            // wizard > list picker > info panel. Each clears its own area.
            if let Some(gate) = self.login_gate.as_ref() {
                gate.render_fullscreen(frame, area);
                return;
            }
            if let Some(overlay_state) = self.setup_overlay.as_mut() {
                overlay_state.render(frame, area);
                return;
            }
            if let Some(wizard) = self.first_run_wizard.as_mut() {
                wizard.render_fullscreen(frame, area);
                return;
            }
            if let Some(picker) = list_picker.as_mut() {
                picker.render(frame, area);
                return;
            }
            if let Some(panel) = info_panel.as_ref() {
                panel.render(frame, area);
                return;
            }

            // Base chat layout (fullscreen): a scrollable chat pane on top, the
            // composer (dynamic 1..=6 text rows, `needed_composer_rows`), and
            // the status bar pinned at the very bottom.
            let inner_w = area.width.saturating_sub(2).max(1);
            // Remember the wrap width so Up/Down key handling moves the caret by
            // the same visual rows the renderer draws.
            context.last_composer_width = inner_w;
            let text_rows = needed_composer_rows(&context.input_buffer, inner_w);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),                // 0: chat pane
                    Constraint::Length(text_rows + 2), // 1: composer / approval prompt
                    Constraint::Length(1),             // 2: status bar
                ])
                .split(area);

            render_chat_pane(state, context, frame, chunks[0]);

            // When an approval is pending, the composer row swaps for the styled
            // approval prompt — same height so the status bar stays put.
            if let Some(ref req) = pending_approval_snapshot {
                render_approval_pane(req, frame, chunks[1]);
            } else {
                render_input_pane(context, frame, chunks[1]);
            }
            render_status_pane(context, state, frame, chunks[2]);

            // /help modal overlays the chat pane but leaves the composer usable.
            if let Some(content) = overlay.as_ref() {
                render_overlay_pane(content, frame, area);
            }

            // Slash-command dropdown, anchored just above the composer.
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

    /// True when a full-screen modal (login gate, list picker, info panel,
    /// autocomplete dropdown, setup overlay, or first-run wizard) currently
    /// owns keyboard input — used to gate paste-burst coalescing so a modal's
    /// own key handling isn't shadowed.
    fn modal_active(&self) -> bool {
        self.login_gate.is_some()
            || self.list_picker.is_some()
            || self.info_panel.is_some()
            || self.autocomplete.is_visible()
            || self.setup_overlay.is_some()
            || self.first_run_wizard.is_some()
    }

    /// Resolve a mouse position to a selection anchor, using the chat-pane
    /// geometry and provenance stashed by the last render. `None` when the
    /// point is outside the pane's inner text area or nothing anchored is
    /// near the hit line.
    fn chat_anchor_at(&self, col: u16, row: u16) -> Option<super::selection::LineAnchor> {
        let area = self.context.last_chat_area?;
        if !super::selection::point_in_chat_inner(col, row, area) {
            return None;
        }
        let gidx = super::selection::global_line_at(row, area, self.context.last_window_start)?;
        super::selection::anchor_at(&self.context.last_chat_provenance, gidx)
    }

    /// Like [`Self::chat_anchor_at`] but clamps the row into the pane's inner
    /// rows first, so a drag that overshoots the border keeps extending the
    /// selection instead of freezing at the edge. The column is ignored —
    /// selection is whole-line.
    fn chat_anchor_at_clamped(&self, _col: u16, row: u16) -> Option<super::selection::LineAnchor> {
        let area = self.context.last_chat_area?;
        if area.height < 3 {
            return None;
        }
        let y0 = area.y + 1;
        let y1 = area.y + area.height - 2;
        let row = row.clamp(y0, y1);
        let gidx = super::selection::global_line_at(row, area, self.context.last_window_start)?;
        super::selection::anchor_at(&self.context.last_chat_provenance, gidx)
    }

    /// Copy the selected transcript text to the system clipboard via OSC 52
    /// and report the outcome on the status line. Honest about the limits:
    /// OSC 52 delivery cannot be confirmed from inside the app, and some
    /// terminals (GNOME Terminal/VTE) don't implement it — the notice says
    /// where to look instead of overclaiming success.
    fn copy_selection(&mut self, sel: super::selection::Selection) {
        let theme = super::render::RenderTheme::default();
        let messages = &self.context.messages;
        let text = super::selection::extract_text(messages.len(), sel.range(), |i| {
            rendered_message_lines(&messages[i], &theme)
        });
        if text.is_empty() {
            self.context.status_notice = Some("nothing to copy".to_string());
            return;
        }
        let n_lines = text.lines().count();
        match super::selection::osc52_sequence(&text) {
            Some(seq) => {
                use std::io::Write as _;
                let mut out = io::stdout();
                let _ = out.write_all(&seq);
                let _ = out.flush();
                self.context.status_notice = Some(format!(
                    "⧉ copied {n_lines} line{} (OSC 52 — clipboard empty? your terminal may \
                     not support it; Shift+drag selects natively)",
                    if n_lines == 1 { "" } else { "s" }
                ));
            }
            None => {
                self.context.status_notice = Some(format!(
                    "selection too large to copy ({} KiB) — select a smaller range",
                    text.len() / 1024
                ));
            }
        }
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
        let lines = render_splash_lines(ctx, size.width, size.height);
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
            for line in render_splash_lines(&self.context, area.width, area.height) {
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
            // Session token total, pretty-printed.
            //
            // `—` when the provider reported nothing. It used to render `0`,
            // which reads as a measured zero; a turn that consumed no tokens
            // does not exist, so the only thing that zero ever meant was
            // "unknown".
            let used_label = self
                .context
                .token_usage
                .as_ref()
                .map_or_else(|| "—".to_string(), |u| format_tokens(u.total_tokens));

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
                Span::styled(used_label, Style::default().fg(Color::Rgb(126, 226, 179))),
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

/// Most memory keys to name before summarising the rest.
///
/// The point of the line is "memory was used, here is roughly what" — past a
/// few names it stops informing and starts crowding the answer it introduces.
const RECALL_KEYS_SHOWN: usize = 3;

/// One line naming the memories that shaped this turn.
///
/// Turn entries from the retired auto-save are counted, not named: their keys
/// are `user_msg_<uuid>`, and a uuid identifies nothing to the person reading
/// it. Naming them was the first version of this line, and it filled the width
/// with hex while saying less than "3 from this conversation" does.
fn render_recalled_memories(keys: &[String]) -> String {
    let noun = if keys.len() == 1 {
        "memory"
    } else {
        "memories"
    };
    if keys.is_empty() {
        // The agent only emits this event with a non-empty block, so an empty
        // key list means the block was built without naming its entries —
        // still worth saying memory was used.
        return "↺ recalled stored memory".to_string();
    }

    let (auto, named): (Vec<&String>, Vec<&String>) =
        keys.iter().partition(|k| crate::memory::is_autosave_key(k));

    let mut parts: Vec<String> = named
        .iter()
        .take(RECALL_KEYS_SHOWN)
        .map(|k| (*k).clone())
        .collect();
    if let Some(rest) = named
        .len()
        .checked_sub(RECALL_KEYS_SHOWN)
        .filter(|n| *n > 0)
    {
        parts.push(format!("+{rest} more"));
    }
    if !auto.is_empty() {
        parts.push(format!("{} from this conversation", auto.len()));
    }

    format!("↺ recalled {} {noun}: {}", keys.len(), parts.join(", "))
}

/// Fit a status line to `width` by dropping whole segments.
///
/// A `Paragraph` clips at the character, so an overflowing line loses the *end*
/// of its last field — which is how "Smart" became "S" at 60 columns, turning
/// the autonomy indicator into an ambiguous letter.
///
/// Each optional segment carries a `drop_rank`: the highest rank goes first.
/// Rank is deliberately separate from position, so deciding what to sacrifice
/// does not reorder what survives.
fn fit_segments<'a>(
    required: Vec<Span<'a>>,
    optional: Vec<(u8, Vec<Span<'a>>)>,
    width: u16,
) -> Vec<Span<'a>> {
    fn cells(spans: &[Span<'_>]) -> usize {
        spans.iter().map(|s| s.content.chars().count()).sum()
    }

    let budget = width as usize;
    let mut kept = optional;

    loop {
        let mut out: Vec<Span<'a>> = required.clone();
        for (_, seg) in &kept {
            out.extend(seg.iter().cloned());
        }
        if cells(&out) <= budget || kept.is_empty() {
            return out;
        }
        // Drop the highest rank, keeping the rest in declaration order.
        let victim = kept
            .iter()
            .enumerate()
            .max_by_key(|(_, (rank, _))| *rank)
            .map(|(i, _)| i);
        match victim {
            Some(i) => {
                kept.remove(i);
            }
            None => return out,
        }
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

/// The one-shot nudge shown when the agent keeps hitting the security gate.
///
/// The preset is named only when the profile has one. The config cannot tell
/// "no preset chosen" from "the default", so naming a preset here told the
/// user a policy was in force that was never selected.
///
/// With no preset, the hint says so in plain words rather than staying quiet
/// about it: silence reads as "a policy is in force but unnamed", which is the
/// same confusion as naming one.
fn shell_block_hint(preset: Option<crate::approval::policy_writer::PolicyPreset>) -> String {
    let subject = match preset {
        Some(preset) => format!("by {} preset", preset.label()),
        None => "by the approval policy — no preset is set".to_string(),
    };
    format!(
        "⚠ Multiple shell commands blocked {subject}. \
         If this is a trusted skill bootstrap, run /autonomy off \
         (then /autonomy smart when done) for unrestricted access."
    )
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

    // `install_log_bridge` is idempotent (one-shot process-global; the
    // second `set_logger` returns Err, which we swallow). main() may have
    // already installed it, so this is the common case.
    crate::logging::install_log_bridge();
    let filter = crate::logging::default_env_filter();
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

/// Split a string into ordered tokens, each token being either a run of
/// non-space chars (a word) or a run of spaces. Used by [`wrap_styled_line`] to
/// break lines on spaces while preserving the exact spacing.
fn tokenize_words(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_is_space: Option<bool> = None;
    for ch in s.chars() {
        let is_sp = ch == ' ';
        if cur_is_space == Some(is_sp) {
            cur.push(ch);
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.push(ch);
            cur_is_space = Some(is_sp);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Word-wrap a styled `Line` to `width` columns, preserving each span's style.
/// Breaks on spaces; a single word longer than `width` is hard-broken. Leading
/// spaces created by a wrap are dropped. Always returns at least one line (an
/// empty input line stays one empty line, so blank paragraph breaks survive).
///
/// Column accounting is one cell per `char` — the same simplification the
/// composer's [`layout_composer`] uses, so wrap width stays consistent across
/// the two panes. Rendering with a plain (non-wrapping) `Paragraph` afterwards
/// means these pre-wrapped lines are never re-wrapped or clipped.
fn wrap_styled_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    // An inter-word space is held here until a following word confirms it stays
    // on the same row, so a space never lands at a wrap point (no trailing
    // spaces, no leading spaces on a wrapped row).
    let mut pending_space: Option<(String, Style)> = None;

    for span in &line.spans {
        let style = span.style;
        for tok in tokenize_words(&span.content) {
            let tw = tok.chars().count();
            if tok.starts_with(' ') {
                if cur_w > 0 {
                    pending_space = Some((tok, style));
                }
                continue;
            }
            let space_w = pending_space.as_ref().map_or(0, |(s, _)| s.chars().count());
            if tw > width {
                // A single word wider than the whole line: start it on a fresh
                // row (dropping any pending space) and hard-break it.
                if cur_w > 0 {
                    out.push(Line::from(std::mem::take(&mut cur)));
                    cur_w = 0;
                }
                pending_space = None;
                for ch in tok.chars() {
                    if cur_w == width {
                        out.push(Line::from(std::mem::take(&mut cur)));
                        cur_w = 0;
                    }
                    cur.push(Span::styled(ch.to_string(), style));
                    cur_w += 1;
                }
            } else if cur_w + space_w + tw > width {
                // Word doesn't fit: wrap, dropping the pending space.
                out.push(Line::from(std::mem::take(&mut cur)));
                cur_w = 0;
                pending_space = None;
                cur.push(Span::styled(tok, style));
                cur_w += tw;
            } else {
                if let Some((s, s_style)) = pending_space.take() {
                    cur_w += s.chars().count();
                    cur.push(Span::styled(s, s_style));
                }
                cur.push(Span::styled(tok, style));
                cur_w += tw;
            }
        }
    }
    // A pending space at end-of-line is intentionally dropped.
    out.push(Line::from(cur));
    out
}

/// A full-width dim horizontal rule for a Markdown thematic break (`---`).
fn horizontal_rule_line(width: usize) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width.max(1)),
        Style::default().fg(Color::Rgb(75, 85, 99)),
    ))
}

/// Selection highlight background — the same blue family as the pane chrome,
/// dark enough that every theme foreground stays readable on it.
const SELECTION_BG: Color = Color::Rgb(40, 70, 140);

/// Rendered (pre-wrap) lines for one transcript message. The SINGLE source
/// both the chat pane and selection-copy extraction use — line indices from
/// one are valid in the other by construction, so a copy can never grab
/// different text than the highlight showed.
fn rendered_message_lines(
    msg: &crate::sessions::Message,
    theme: &super::render::RenderTheme,
) -> Vec<Line<'static>> {
    let persisted = msg
        .tool_calls
        .as_deref()
        .map(super::render::parse_persisted_tool_calls)
        .unwrap_or_default();
    super::render::render_message_lines(&msg.role, &msg.content, &persisted, &[], theme)
}

/// Build the full display-line buffer for a non-empty transcript, plus the
/// per-display-line provenance the selection feature depends on. A rendered
/// line that wraps into N display lines yields N identical anchors; turn
/// separators, the spinner, and the volatile streaming tail carry `None`.
fn build_transcript_lines(
    messages: &[crate::sessions::Message],
    state: &AppState,
    inner_w: usize,
    theme: &super::render::RenderTheme,
) -> (
    Vec<Line<'static>>,
    Vec<Option<super::selection::LineAnchor>>,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut prov: Vec<Option<super::selection::LineAnchor>> = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        // A blank line between turns so a "You:" and the reply above it don't
        // run together — one separator, never a trailing one.
        if i > 0 {
            lines.push(Line::from(""));
            prov.push(None);
        }
        let raw = rendered_message_lines(msg, theme);
        for (raw_idx, rline) in raw.iter().enumerate() {
            let before = lines.len();
            extend_wrapped(&mut lines, std::slice::from_ref(rline), inner_w);
            prov.extend(std::iter::repeat_n(
                Some((i, raw_idx)),
                lines.len() - before,
            ));
        }
    }

    if let AppState::Streaming {
        partial,
        tool_blocks,
        ..
    } = state
    {
        if !messages.is_empty() {
            lines.push(Line::from(""));
            prov.push(None);
        }
        let frame_idx = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as usize / 100)
            .unwrap_or(0))
            % SPINNER_FRAMES.len();
        let spinner = SPINNER_FRAMES[frame_idx];
        lines.push(Line::from(vec![
            Span::styled(
                format!("{spinner} "),
                Style::default()
                    .fg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("thinking…", Style::default().fg(Color::Rgb(107, 114, 128))),
        ]));
        prov.push(None);
        let raw =
            super::render::render_message_lines("assistant", partial, &[], tool_blocks, theme);
        let before = lines.len();
        extend_wrapped(&mut lines, &raw, inner_w);
        prov.extend(std::iter::repeat_n(None, lines.len() - before));
    }

    (lines, prov)
}

#[cfg(test)]
mod transcript_provenance_tests {
    use super::*;
    use crate::sessions::Message;

    fn theme() -> super::super::render::RenderTheme {
        super::super::render::RenderTheme::default()
    }

    #[test]
    fn provenance_length_matches_lines_for_mixed_transcript() {
        let msgs = vec![
            Message::user("s", "hello"),
            Message::assistant("s", "line one\nline two"),
        ];
        let (lines, prov) = build_transcript_lines(&msgs, &AppState::Ready, 40, &theme());
        assert_eq!(lines.len(), prov.len());
        // 2 rendered lines (label+first, none extra) for msg 0? label line only.
        // Exact counts: msg0 = 1 line, separator = 1, msg1 = 2 lines.
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn separator_rows_carry_no_anchor() {
        let msgs = vec![Message::user("s", "a"), Message::assistant("s", "b")];
        let (_, prov) = build_transcript_lines(&msgs, &AppState::Ready, 40, &theme());
        assert_eq!(prov[0], Some((0, 0)));
        assert_eq!(prov[1], None, "turn separator must not be selectable");
        assert_eq!(prov[2], Some((1, 0)));
    }

    #[test]
    fn a_wrapped_rendered_line_yields_identical_consecutive_anchors() {
        // Narrow pane: one long rendered line wraps into several display lines,
        // every one of which must anchor back to the SAME rendered line.
        let msgs = vec![Message::user(
            "s",
            "alpha beta gamma delta epsilon zeta eta theta",
        )];
        let (lines, prov) = build_transcript_lines(&msgs, &AppState::Ready, 12, &theme());
        assert!(lines.len() > 2, "expected the line to wrap, got {lines:?}");
        for p in &prov {
            assert_eq!(*p, Some((0, 0)), "all wraps anchor to rendered line 0");
        }
    }

    #[test]
    fn streaming_tail_and_spinner_carry_no_anchor() {
        let msgs = vec![Message::user("s", "q")];
        let state = AppState::Streaming {
            partial: "partial reply".into(),
            tool_blocks: Vec::new(),
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };
        let (lines, prov) = build_transcript_lines(&msgs, &state, 40, &theme());
        assert_eq!(lines.len(), prov.len());
        // Everything after the committed message (separator, spinner, partial)
        // must be None — the tail is volatile mid-stream.
        assert_eq!(prov[0], Some((0, 0)));
        assert!(
            prov[1..].iter().all(Option::is_none),
            "streaming tail must be unselectable, got {prov:?}"
        );
    }
}

/// Append the wrapped form of each rendered message line to `out`, drawing a
/// Markdown horizontal rule (`---`/`***`/`___`) as a full-width rule instead of
/// wrapping its literal characters.
fn extend_wrapped(out: &mut Vec<Line<'static>>, raw: &[Line<'static>], inner_w: usize) {
    for line in raw {
        let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if super::render::is_horizontal_rule(&plain) {
            out.push(horizontal_rule_line(inner_w));
        } else {
            out.extend(wrap_styled_line(line, inner_w));
        }
    }
}

fn render_chat_pane(
    state: &AppState,
    ctx: &mut TuiContext,
    frame: &mut ratatui::Frame,
    area: Rect,
) {
    let theme = super::render::RenderTheme::default();
    let inner_w = area.width.saturating_sub(2).max(1) as usize;
    let inner_h = (area.height.saturating_sub(2).max(1)) as usize;
    ctx.last_chat_rows = inner_h;
    // Stash the pane rect so mouse hit-testing maps clicks against exactly
    // the frame the user sees (selection::point_in_chat_inner).
    ctx.last_chat_area = Some(area);

    // Flatten every message into individual, pre-wrapped display lines. A plain
    // `List` renders NOTHING when a single item (one long message) is taller
    // than the pane, so we can't hand it multi-line items; instead we build one
    // flat line buffer, word-wrap it to the pane width (so long replies reflow
    // instead of being clipped off the right edge), and window the bottom
    // `inner_h` lines ourselves. `provenance` tracks, per display line, which
    // message's rendered line it wraps out of — the anchor space selection
    // lives in (None = separator / spinner / streaming tail / splash).
    let streaming = matches!(state, AppState::Streaming { .. });
    let (mut lines, provenance) = if ctx.messages.is_empty() && !streaming {
        // The splash is ASCII art already sized to the pane width; don't
        // re-wrap it (that would mangle the logo).
        let splash = render_splash_lines(ctx, area.width, area.height);
        let prov = vec![None; splash.len()];
        (splash, prov)
    } else {
        build_transcript_lines(&ctx.messages, state, inner_w, &theme)
    };

    // Paint the active selection as a whole-line background over every
    // display line whose provenance falls inside the range.
    if let Some(sel) = ctx.selection {
        for (idx, p) in provenance.iter().enumerate() {
            if p.is_some_and(|a| sel.contains(a)) {
                if let Some(line) = lines.get_mut(idx) {
                    for span in &mut line.spans {
                        span.style = span.style.bg(SELECTION_BG);
                    }
                }
            }
        }
    }

    // Stick to the bottom: show the last `inner_h` lines. `scroll_offset` is how
    // many lines up from the newest the user has paged with PgUp (0 = bottom).
    // Clamp it here so it can never scroll past the top and PgDn always returns.
    let total = lines.len();
    let max_scroll = total.saturating_sub(inner_h);
    if ctx.scroll_offset > max_scroll {
        ctx.scroll_offset = max_scroll;
    }
    let end = total.saturating_sub(ctx.scroll_offset);
    let start = end.saturating_sub(inner_h);
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    // Stash the window start + provenance so the mouse handler resolves a
    // clicked row to the same display line this frame drew there.
    ctx.last_window_start = start;
    ctx.last_chat_provenance = provenance;

    let para = Paragraph::new(visible).block(
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
    frame.render_widget(para, area);
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
        let row_count = u16::try_from(single.line_count(width).max(1)).unwrap_or(u16::MAX);

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
/// │ [Y] yes (session)   [A] always   [N] no   [Esc] deny │
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
    chip_spans.extend(chip("Y", "yes (session)", key_bg, key_fg, muted));
    // Says what it grants. "always (persist)" reads as "this command";
    // allowlisting one long `curl` invocation permits EVERY future `curl`.
    let always_label = format!("always: any `{}`", req.basename);
    chip_spans.extend(chip("A", &always_label, key_bg, key_fg, muted));
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
        // Where it came from. An approval arriving from a chat channel was
        // indistinguishable from one the operator triggered locally.
        Span::styled(
            format!(" · from {} ", req.channel),
            Style::default().fg(muted),
        ),
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

/// Mounting a `TuiApp` for tests.
///
/// `TuiApp` has fifty-odd fields, so every test module that needed one wrote
/// its own constructor — four byte-identical fifty-line copies. That is why
/// behaviour reachable only through `handle_key` went untested: not because it
/// was unreachable, but because reaching it meant maintaining a fifth copy.
///
/// **Point the profile at a temporary directory for anything that writes.**
/// The copies this replaces all set `root` to the operator's real profile
/// directory, so a test that reached `cycle_autonomy_preset` would have
/// rewritten the machine's own `policy/` files. [`app_with_profile_root`] is
/// the constructor to use whenever the code under test can touch disk.
#[cfg(test)]
pub(super) mod test_support {
    use super::{AppState, CommandRegistry, SetupChange, TuiApp, INLINE_VIEWPORT_LINES};
    use crate::tui::context::TuiContext;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// A `TuiApp` in `Ready` state whose profile lives under `profile_root`.
    pub(crate) fn app_with_profile_root(ctx: TuiContext, profile_root: PathBuf) -> TuiApp {
        // `Config::default()` points `config_path` at the developer's real
        // `$HOME/.rantaiclaw/config.toml`. A test that calls `save()` on this
        // app would write there, so point it at the same profile root the
        // caller already gives us (a tempdir for every test that writes).
        let mut config = crate::config::Config::default();
        config.config_path = profile_root.join("config.toml");
        TuiApp {
            state: AppState::Ready,
            context: ctx,
            command_registry: CommandRegistry::new(),
            config,
            profile: crate::profile::Profile {
                name: "default".to_string(),
                root: profile_root,
            },
            autocomplete: crate::tui::widgets::Autocomplete::new(),
            overlay: None,
            setup_overlay: None,
            setup_event_rx: None,
            setup_save_complete_rx: None,
            pending_setup_change: SetupChange::None,
            channels_restarted_for_save: false,
            setup_response_tx: None,
            setup_force: false,
            scrollback_queue: Vec::new(),
            selection_drag_anchor: None,
            selection_dragged: false,
            list_picker: None,
            clawhub_install_last_query: String::new(),
            clawhub_install_search_version: 0,
            clawhub_install_results_rx: None,
            clawhub_install_results_tx: None,
            clawhub_install_in_progress: None,
            clawhub_search_frame: None,
            clawhub_install_completion_rx: None,
            clawhub_install_completion_tx: None,
            skill_deps_install_in_progress: None,
            skill_deps_install_completion_rx: None,
            skill_deps_install_completion_tx: None,
            skill_deps_install_finished_at: None,
            skills_watcher: None,
            config_watcher: None,
            config_fingerprint: Arc::new(std::sync::Mutex::new(String::new())),
            model_switch_save_rx: None,
            channel_supervisor: None,
            info_panel: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            skill_editor_request: None,
            clear_terminal_request: false,
            composer_viewport_rows: INLINE_VIEWPORT_LINES,
            first_run_wizard: None,
            login_gate: None,
            pending_approvals_rx: None,
            shell_blocks_this_turn: 0,
            autonomy_hint_shown_this_turn: false,
            pending_turns: std::collections::VecDeque::new(),
            actor_has_agent: true,
            pending_approval: None,
            pending_reload: None,
            reload_seq: 0,
            model_picker_entries: None,
        }
    }

    /// The form for tests that never write: the profile root is a `default`
    /// directory under one temp dir shared by the whole test process, never a
    /// path derived from `HOME`. A test that writes should call
    /// [`app_with_profile_root`] with its own temp dir instead, so its files do
    /// not leak into a sibling test's app.
    pub(crate) fn app_with_context(ctx: TuiContext) -> TuiApp {
        static SHARED_ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let root = SHARED_ROOT.get_or_init(|| {
            // A static `TempDir` never drops, so the pid in the name lets a later
            // run prune the directories of test processes that have exited.
            const PREFIX: &str = "rantaiclaw-tui-test-profile-";
            crate::profile::dev_guard::prune_dead_pid_dirs(&std::env::temp_dir(), PREFIX);
            tempfile::Builder::new()
                .prefix(&format!("{PREFIX}{}-", std::process::id()))
                .tempdir()
                .expect("shared tui test profile root")
        });
        app_with_profile_root(ctx, root.path().join("profiles").join("default"))
    }
}

#[cfg(test)]
mod skill_body_validation_tests {
    use super::{validate_skill_body, SkillEditRequest};
    use std::path::PathBuf;

    fn req(is_new: bool, slug: &str, initial: &str) -> SkillEditRequest {
        SkillEditRequest {
            slug: slug.to_string(),
            path: PathBuf::from("/tmp").join(slug).join("SKILL.md"),
            initial: initial.to_string(),
            is_new,
        }
    }

    const EXISTING: &str = "---\nname: Kopi Pagi\ndescription: x\n---\n\n# Kopi Pagi\n";

    #[test]
    fn accepts_an_unchanged_edit() {
        let r = req(false, "kopi-pagi", EXISTING);
        let body = format!("{EXISTING}\n## Troubleshooting\nToo sour: grind finer.\n");
        assert!(validate_skill_body(&body, &r).is_ok());
    }

    #[test]
    fn rejects_a_body_the_loader_could_not_read() {
        let r = req(false, "kopi-pagi", EXISTING);
        for body in [
            "no frontmatter\n",
            "---\ndescription: x\n---\n",
            "---\nname:   \n---\n",
        ] {
            let err = validate_skill_body(body, &r).expect_err("should reject");
            assert!(err.contains("name"), "{body:?} -> {err}");
        }
    }

    #[test]
    fn rejects_a_rename_including_a_case_only_one() {
        let r = req(false, "kopi-pagi", EXISTING);
        // A case change keeps the same folder but orphans the
        // `[skills.entries.<name>]` config key, silently resetting whether the
        // skill is enabled — so it is a rename too.
        for renamed in ["kopi pagi", "Kopi Pagi Baru", "KOPI PAGI"] {
            let body = format!("---\nname: {renamed}\n---\n# x\n");
            let err = validate_skill_body(&body, &r).expect_err("should reject {renamed}");
            assert!(err.contains("renaming"), "{renamed} -> {err}");
        }
    }

    #[test]
    fn new_skill_must_keep_the_folder_it_was_staged_for() {
        let r = req(true, "kopi-pagi", "");
        assert!(validate_skill_body("---\nname: Kopi Pagi\n---\n# x\n", &r).is_ok());
        // Renaming in the editor would leave the folder saying one thing and
        // the manifest another.
        let err =
            validate_skill_body("---\nname: Teh Sore\n---\n# x\n", &r).expect_err("should reject");
        assert!(err.contains("teh-sore"), "{err}");
    }
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
            let text = composer_body(buffer, 80);
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
            let text = composer_body(buffer, 80);
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
        assert_eq!(composer_body("a\nb", 80).lines.len(), 2, "LF");
        assert_eq!(composer_body("a\rb", 80).lines.len(), 2, "CR");
        assert_eq!(composer_body("a\r\nb", 80).lines.len(), 2, "CRLF");
    }

    #[test]
    fn splits_one_line_per_logical_line() {
        assert_eq!(composer_body("a\nb\nc", 80).lines.len(), 3);
        assert_eq!(composer_body("single", 80).lines.len(), 1);
    }

    /// A trailing newline is where the cursor sits after `Ctrl+J`; dropping
    /// the empty final line (as `str::lines` would) hides it.
    #[test]
    fn trailing_newline_keeps_its_empty_final_line() {
        let text = composer_body("body\n", 80);
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
        let text = composer_body("", 80);
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
        let text = composer_body("first\nsecond\nthird", 80);
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

    /// A logical line longer than the inner width is split into multiple
    /// display rows by our own wrapping (ratatui word-wrap is gone). Row 0
    /// budget is inner_w - 2 (prefix); here inner_w=12 → 10 chars on row 0.
    #[test]
    fn a_long_line_wraps_into_multiple_display_rows() {
        let text = super::composer_body("abcdefghijklmn", 12); // 14 chars
        assert_eq!(text.lines.len(), 2);
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

    /// The caret cell must index a real position inside the rendered rows:
    /// caret_col is a valid column of the caret_row that layout_composer
    /// emits (accounting for the 2-cell prefix on row 0). This is the
    /// invariant that failed when the caret used char-wrap and the renderer
    /// used ratatui word-wrap.
    #[test]
    fn caret_indexes_a_real_cell_in_the_rendered_rows() {
        use super::layout_composer;
        let inner_w = 12u16;
        // A logical line that must wrap (14 chars > inner_w-prefix=10).
        let buf = "abcdefghijklmn";
        for cursor in 0..=buf.chars().count() {
            let l = layout_composer(buf, cursor, inner_w);
            assert!(
                (l.caret_row as usize) < l.rows.len(),
                "cursor {cursor}: caret_row {} out of {} rows",
                l.caret_row,
                l.rows.len()
            );
            // Row 0 carries a 2-cell prefix, so its text width budget is
            // inner_w-2; other rows use the full inner_w. caret_col must not
            // exceed the row's used width.
            let prefix = if l.caret_row == 0 { 2 } else { 0 };
            let row_len =
                u16::try_from(l.rows[l.caret_row as usize].chars().count()).unwrap_or(u16::MAX);
            assert!(
                l.caret_col <= prefix + row_len,
                "cursor {cursor}: caret_col {} beyond row width {}",
                l.caret_col,
                prefix + row_len
            );
        }
    }

    #[test]
    fn needed_rows_clamps_between_one_and_six() {
        use super::needed_composer_rows;
        assert_eq!(needed_composer_rows("", 40), 1, "empty is one row");
        assert_eq!(needed_composer_rows("one line", 40), 1);
        assert_eq!(needed_composer_rows("a\nb\nc", 40), 3);
        // 20 logical lines clamp to the cap of 6.
        let many = (0..20).map(|_| "x").collect::<Vec<_>>().join("\n");
        assert_eq!(needed_composer_rows(&many, 40), 6);
    }

    #[test]
    fn visual_move_steps_between_logical_lines_keeping_column() {
        use super::visual_cursor_move;
        // 3 lines: "abc" (3), "defgh" (5), "ij" (2). Wide enough not to wrap.
        let buf = "abc\ndefgh\nij";
        let inner_w = 40u16;
        // Caret on the middle line at screen cell 4 ("defg|h") = index 8
        // (d..h occupy cells 0..4, no prefix on a continuation/lower line).
        let mid = 4 + 4;
        // Up → first line. Row 0 carries the 2-cell prefix, so screen cell 4
        // there points between "ab" and "c" ("ab|c") = index 2. Same screen
        // column, different char index — that is correct visual movement.
        assert_eq!(visual_cursor_move(buf, mid, inner_w, true), Some(2));
        // Down from that position → back onto the middle line at the same cell.
        let up = visual_cursor_move(buf, mid, inner_w, true).unwrap();
        assert_eq!(visual_cursor_move(buf, up, inner_w, false), Some(mid));
        // Down from the middle line → last line "ij" (clamped to its length).
        assert_eq!(
            visual_cursor_move(buf, mid, inner_w, false),
            Some(4 + 6 + 2)
        );
    }

    #[test]
    fn visual_move_navigates_soft_wrapped_rows_of_one_long_line() {
        use super::{layout_composer, visual_cursor_move};
        // One logical line, no `\n`, forced to wrap: inner_w 12 → 10 cells on
        // row 0 (2-cell prefix) then 12 per row. 30 chars spans 3 visual rows.
        let buf: String = "abcdefghijklmnopqrstuvwxyz0123".chars().collect();
        let inner_w = 12u16;
        // Caret at the end (row 2). Up must reach row 1 (a wrapped row), NOT
        // bail to history — this is the whole point of visual-row movement.
        let end = buf.chars().count();
        let end_row = layout_composer(&buf, end, inner_w).caret_row;
        assert!(end_row >= 2, "buffer should span >=3 rows, got {end_row}");
        let up1 = visual_cursor_move(&buf, end, inner_w, true).expect("row above end");
        assert_eq!(layout_composer(&buf, up1, inner_w).caret_row, end_row - 1);
        let up2 = visual_cursor_move(&buf, up1, inner_w, true).expect("row 0");
        assert_eq!(layout_composer(&buf, up2, inner_w).caret_row, 0);
        // On the first visual row Up bails (None) so the caller hits history.
        assert_eq!(visual_cursor_move(&buf, up2, inner_w, true), None);
    }

    #[test]
    fn visual_move_single_row_defers_to_history() {
        use super::visual_cursor_move;
        let buf = "just one line";
        let inner_w = 40u16;
        let end = buf.chars().count();
        // Only one visual row → both directions return None (history fallback).
        assert_eq!(visual_cursor_move(buf, end, inner_w, true), None);
        assert_eq!(visual_cursor_move(buf, end, inner_w, false), None);
    }

    fn wrap_plain(text: &str, width: usize) -> Vec<String> {
        use super::wrap_styled_line;
        let line = ratatui::text::Line::from(text.to_string());
        wrap_styled_line(&line, width)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn wrap_breaks_on_word_boundaries_within_width() {
        let rows = wrap_plain("alpha bravo charlie", 12);
        assert_eq!(rows, vec!["alpha bravo".to_string(), "charlie".to_string()]);
        for r in &rows {
            assert!(r.chars().count() <= 12, "row {r:?} exceeds width");
        }
    }

    #[test]
    fn wrap_hard_breaks_a_word_longer_than_width() {
        // 10-char word into width 4 → 4 + 4 + 2.
        let rows = wrap_plain("abcdefghij", 4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_preserves_blank_and_short_lines_unchanged() {
        assert_eq!(wrap_plain("", 10), vec![String::new()]);
        assert_eq!(wrap_plain("fits", 10), vec!["fits".to_string()]);
    }

    #[test]
    fn wrap_preserves_span_styles() {
        use super::wrap_styled_line;
        use ratatui::style::{Color, Style};
        use ratatui::text::{Line, Span};
        // A styled label span then plain body, forced to wrap.
        let red = Style::default().fg(Color::Red);
        let line = Line::from(vec![
            Span::styled("Label: ".to_string(), red),
            Span::raw("one two three four five".to_string()),
        ]);
        let rows = wrap_styled_line(&line, 10);
        assert!(rows.len() > 1, "should wrap onto multiple rows");
        // The label span keeps its colour on row 0.
        assert_eq!(rows[0].spans[0].content.as_ref(), "Label:");
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Red));
    }
}

#[cfg(test)]
mod paste_coalesce_tests {
    use super::{batch_is_paste, key_to_text, TextKey};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn k(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn plain_char_maps_to_text() {
        assert!(matches!(
            key_to_text(&k(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(TextKey::Ch('x'))
        ));
    }

    #[test]
    fn enter_and_tab_map_to_newline_and_tab() {
        assert!(matches!(
            key_to_text(&k(KeyCode::Enter, KeyModifiers::NONE)),
            Some(TextKey::Newline)
        ));
        assert!(matches!(
            key_to_text(&k(KeyCode::Tab, KeyModifiers::NONE)),
            Some(TextKey::Tab)
        ));
    }

    #[test]
    fn control_char_is_not_text() {
        // Ctrl+C, Ctrl+J etc. must stay on the command path, never a paste.
        assert!(key_to_text(&k(KeyCode::Char('c'), KeyModifiers::CONTROL)).is_none());
        assert!(key_to_text(&k(KeyCode::Esc, KeyModifiers::NONE)).is_none());
    }

    #[test]
    fn a_batch_is_a_paste_when_multi_char_or_has_a_newline() {
        assert!(batch_is_paste("ab")); // two chars
        assert!(batch_is_paste("a\nb")); // contains newline
        assert!(batch_is_paste("\n")); // count()==1, isolates the contains('\n') clause
        assert!(!batch_is_paste("a")); // lone char is normal typing
        assert!(!batch_is_paste("")); // empty
    }

    /// Regression: the placeholder trigger must count WRAPPED rows, not raw
    /// `\n`s. Three long prose paragraphs separated by blank lines are only 5
    /// raw lines yet flood the composer; the old `split('\n').count() > 5`
    /// check let them land inline with no `[Pasted text]` marker.
    #[test]
    fn estimated_rows_counts_wrapped_paragraphs_not_just_newlines() {
        use super::estimated_paste_rows;
        // Short note stays small → lands inline.
        assert!(estimated_paste_rows("hi\nthere") <= 5);
        // Three ~140-char paragraphs, blank-line separated: 5 raw lines but wide.
        let para = "x".repeat(140);
        let three_paras = format!("{para}\n\n{para}\n\n{para}");
        assert_eq!(
            three_paras.split('\n').count(),
            5,
            "raw line count is only 5"
        );
        assert!(
            estimated_paste_rows(&three_paras) > 5,
            "three long paragraphs must estimate > 5 wrapped rows"
        );
        // A single very long line is also large.
        assert!(estimated_paste_rows(&"y".repeat(500)) > 5);
        // Many short lines stay large (line-driven, unchanged behavior).
        assert!(estimated_paste_rows("a\nb\nc\nd\ne\nf\ng\nh") > 5);
    }

    #[test]
    fn leading_text_key_excludes_enter_and_tab_but_keeps_chars() {
        use super::leading_text_key;
        use crossterm::event::Event;
        // A leading Enter or Tab must NOT start a coalesce batch (they fall
        // through to handle_key: Enter submits, Tab reaches nav/no-op).
        assert!(leading_text_key(&Event::Key(k(KeyCode::Enter, KeyModifiers::NONE))).is_none());
        assert!(leading_text_key(&Event::Key(k(KeyCode::Tab, KeyModifiers::NONE))).is_none());
        // A plain printable char still starts a batch.
        assert!(leading_text_key(&Event::Key(k(KeyCode::Char('x'), KeyModifiers::NONE))).is_some());
    }
}

/// A key event reduced to the text it contributes to a paste batch.
///
/// Only unmodified printable input counts: `Char` without Ctrl (Shift/Alt
/// carry real text and are kept), plus `Enter`/`Tab`. Everything else —
/// Ctrl chords, arrows, Esc, function keys — is `None` so it stays on the
/// command path and never gets swallowed into a paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKey {
    Ch(char),
    Newline,
    Tab,
}

fn key_to_text(key: &crossterm::event::KeyEvent) -> Option<TextKey> {
    use crossterm::event::{KeyCode, KeyModifiers};
    match key.code {
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => Some(TextKey::Ch(c)),
        KeyCode::Enter => Some(TextKey::Newline),
        KeyCode::Tab => Some(TextKey::Tab),
        _ => None,
    }
}

/// A drained key batch is a paste (route it through the paste path) when it
/// carries more than one char or any newline. A lone char is ordinary typing.
fn batch_is_paste(batch: &str) -> bool {
    batch.chars().count() > 1 || batch.contains('\n')
}

/// Estimate how many *wrapped* terminal rows a pasted block would occupy in the
/// composer, wrapping each logical line at a nominal width. Deciding whether a
/// paste is "large enough to collapse" by raw `\n` count alone misses a few
/// very long paragraphs (lots of text, few newlines — the reported case: three
/// prose paragraphs are only 5 raw lines yet wrap far past the composer's cap);
/// estimating wrapped rows catches both tall (many lines) and wide (long lines)
/// pastes. A nominal 80-col width is used because the real terminal width is not
/// available on every paste path and the estimate only needs to be a threshold,
/// not exact.
fn estimated_paste_rows(text: &str) -> usize {
    const NOMINAL_WIDTH: usize = 80;
    text.split('\n')
        .map(|line| line.chars().count().div_ceil(NOMINAL_WIDTH).max(1))
        .sum()
}

/// Text key allowed to START a coalesced batch: a printable char, but NOT
/// Enter or Tab — a lone Enter must reach the normal submit path, and a lone
/// Tab must reach `handle_key` (autocomplete/overlay nav or no-op) instead of
/// being inserted as a literal `\t`.
fn leading_text_key(ev: &Event) -> Option<TextKey> {
    match ev {
        Event::Key(k) if k.kind == crossterm::event::KeyEventKind::Press => match key_to_text(k) {
            Some(TextKey::Newline | TextKey::Tab) => None,
            other => other,
        },
        _ => None,
    }
}

/// Text key allowed INSIDE a batch already started by a printable key: any
/// text key, Enter included (it becomes '\n').
fn leading_or_inner_text_key(ev: &Event) -> Option<TextKey> {
    match ev {
        Event::Key(k) if k.kind == crossterm::event::KeyEventKind::Press => key_to_text(k),
        _ => None,
    }
}

fn push_text_key(buf: &mut String, tk: TextKey) {
    match tk {
        TextKey::Ch(c) => buf.push(c),
        TextKey::Newline => buf.push('\n'),
        TextKey::Tab => buf.push('\t'),
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

/// The composer laid out as terminal display rows plus the caret cell.
///
/// Single source of truth: the renderer draws exactly `rows` and the
/// terminal cursor is placed at `(caret_row, caret_col)`, both computed by
/// one walk. Because render and caret share this function, the cursor can
/// never drift off the real character — the skew that existed when the
/// caret hard-wrapped at `inner_w` while ratatui word-wrapped is gone.
pub struct ComposerLayout {
    /// Display rows, prefix-free. The renderer prepends the `"▎ "` prefix to
    /// row 0 only; continuation and wrapped rows start at column 0.
    pub rows: Vec<String>,
    pub caret_row: u16,
    pub caret_col: u16,
}

/// Wrap `buffer` into display rows and locate the caret for `cursor_pos`.
///
/// Char-cell model: each char consumes one cell, hard-breaking at `inner_w`.
/// Row 0 starts at column `prefix` (2 cells for `"▎ "`), so it holds
/// `inner_w - prefix` chars before wrapping; every later row holds `inner_w`.
/// Newlines (already normalised to `\n`) start a fresh row at column 0.
pub fn layout_composer(buffer: &str, cursor_pos: usize, inner_w: u16) -> ComposerLayout {
    let inner_w = inner_w.max(1);
    let prefix: u16 = 2;
    let normalized = normalize_line_breaks(buffer);

    let mut rows: Vec<String> = vec![String::new()];
    // Cells used on the current row, INCLUDING the prefix on row 0.
    let mut col: u16 = prefix.min(inner_w.saturating_sub(1));
    let mut caret_row: u16 = 0;
    let mut caret_col: u16 = col;

    for (i, ch) in normalized.chars().enumerate() {
        if i == cursor_pos {
            caret_row = u16::try_from(rows.len() - 1).unwrap_or(u16::MAX);
            caret_col = col;
        }
        if ch == '\n' {
            rows.push(String::new());
            col = 0;
        } else {
            rows.last_mut().expect("rows always non-empty").push(ch);
            col = col.saturating_add(1);
            if col >= inner_w {
                rows.push(String::new());
                col = 0;
            }
        }
    }
    if cursor_pos >= normalized.chars().count() {
        caret_row = u16::try_from(rows.len() - 1).unwrap_or(u16::MAX);
        caret_col = col;
    }

    ComposerLayout {
        rows,
        caret_row,
        caret_col: caret_col.min(inner_w.saturating_sub(1)),
    }
}

/// Caret cell for `cursor_pos` as `(row, col)` inside the composer's inner
/// area, before any scroll is applied. Delegates to [`layout_composer`] so
/// the caret and the renderer share one wrap model.
fn composer_caret_cell(buffer: &str, cursor_pos: usize, inner_w: u16) -> (u16, u16) {
    let l = layout_composer(buffer, cursor_pos, inner_w);
    (l.caret_row, l.caret_col)
}

/// New cursor position after moving the caret one VISUAL row up (`up = true`)
/// or down, keeping the target column as close as possible. Visual rows include
/// soft-wrapped rows, so a single long line that wraps still navigates row by
/// row — matching a normal terminal editor, not just movement between `\n`
/// separated lines. Returns `None` when there is no row above/below the caret,
/// so the caller falls back to history recall (Up on the first row recalls an
/// older prompt; Down on the last row recalls a newer one).
///
/// Shares [`layout_composer`] as the single wrap model so caret placement here
/// always agrees with what the renderer draws.
fn visual_cursor_move(buffer: &str, cursor_pos: usize, inner_w: u16, up: bool) -> Option<usize> {
    let cur = layout_composer(buffer, cursor_pos, inner_w);
    let target_row = if up {
        cur.caret_row.checked_sub(1)?
    } else {
        cur.caret_row.saturating_add(1)
    };
    let n = buffer.chars().count();
    // caret_row is monotonic non-decreasing as `p` grows, so we can stop once we
    // pass the target row. Pick the position on the target row whose column is
    // closest to the current column.
    let mut best: Option<(usize, u16)> = None;
    for p in 0..=n {
        let l = layout_composer(buffer, p, inner_w);
        if l.caret_row == target_row {
            let dist = l.caret_col.abs_diff(cur.caret_col);
            let better = match best {
                Some((_, best_dist)) => dist < best_dist,
                None => true,
            };
            if better {
                best = Some((p, dist));
            }
        } else if l.caret_row > target_row {
            break;
        }
    }
    best.map(|(p, _)| p)
}

/// Number of composer TEXT rows the buffer needs, clamped to the 1..=6 cap.
/// Drives the dynamic inline-viewport height.
fn needed_composer_rows(buffer: &str, inner_w: u16) -> u16 {
    let rows = layout_composer(buffer, 0, inner_w).rows.len();
    // Clamp in `usize` first (result is 1..=6), so the narrowing `try_from`
    // can never fail; `unwrap_or(6)` is a dead-safe fallback that also keeps
    // clippy's pedantic cast lint happy.
    u16::try_from(rows.clamp(1, 6)).unwrap_or(6)
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
fn composer_body(input_buffer: &str, inner_w: u16) -> Text<'static> {
    let prefix = Span::styled(
        "▎ ",
        Style::default()
            .fg(Color::Rgb(94, 184, 255))
            .add_modifier(Modifier::BOLD),
    );

    if input_buffer.is_empty() {
        // The hint used to be one string that the widget clipped mid-phrase: at
        // 80 columns it ended "· Ctrl+C" with an unclosed parenthesis and no
        // "exit", so the one thing a stuck operator needs was the first to go.
        // Drop whole hints instead, least useful first, and only bracket what
        // actually survived.
        let hint_style = Style::default().fg(Color::Rgb(107, 114, 128));
        const PLACEHOLDER: &str = "Type a message…";
        // Display order is reading order — primary action first, escape last —
        // and the rank decides what a narrow terminal gives up. Keeping those
        // separate matters: sorting by importance would put "Ctrl+C exit" at the
        // front of the sentence, which reads like a warning rather than a hint.
        const HINTS: [(u8, &str); 4] = [
            (1, "Enter sends"),
            (2, "/help for commands"),
            (3, "Ctrl+J newline"),
            // Last to go: without it, someone who cannot get out has nothing.
            (0, "Ctrl+C exit"),
        ];

        // 2 for the prefix, 4 for the "  (" and ")" around the hints.
        let budget = usize::from(inner_w).saturating_sub(PLACEHOLDER.chars().count() + 6);
        let mut kept: Vec<(u8, &str)> = HINTS.to_vec();
        let width_of = |hs: &[(u8, &str)]| -> usize {
            hs.iter().map(|(_, h)| h.chars().count()).sum::<usize>()
                + 3 * hs.len().saturating_sub(1)
        };
        while kept.len() > 1 && width_of(&kept) > budget {
            if let Some(i) = kept
                .iter()
                .enumerate()
                .max_by_key(|(_, (rank, _))| *rank)
                .map(|(i, _)| i)
            {
                kept.remove(i);
            } else {
                break;
            }
        }

        let joined = kept.iter().map(|(_, h)| *h).collect::<Vec<_>>().join(" · ");
        let text = if joined.chars().count() <= budget {
            format!("{PLACEHOLDER}  ({joined})")
        } else {
            PLACEHOLDER.to_string()
        };

        return Text::from(Line::from(vec![prefix, Span::styled(text, hint_style)]));
    }

    // Same wrapping the caret walker uses (cursor_pos irrelevant here).
    let rows = layout_composer(input_buffer, 0, inner_w).rows;
    let lines: Vec<Line<'static>> = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let text = Span::raw(row);
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

    let input = Paragraph::new(composer_body(&ctx.input_buffer, inner_w))
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
        );
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
    } else if let Some(ref notice) = ctx.status_notice {
        // Transient neutral notice (e.g. selection-copy feedback). Errors win
        // the line; the notice clears on the next submit.
        Line::from(vec![
            Span::styled(" · ", sky),
            Span::styled(notice.clone(), muted),
        ])
    } else {
        // `—` when nothing was reported — see the session token total in
        // render_status above for why a zero here would be a claim rather
        // than a measurement.
        let used_label = ctx
            .token_usage
            .as_ref()
            .map_or_else(|| "—".to_string(), |u| format_tokens(u.total_tokens));
        let age_secs = ctx.started_at.elapsed().as_secs();
        let age_label = format_duration_short(age_secs);

        // The autonomy indicator tells the operator whether tools will stop and
        // ask. It used to sit last, so a narrow terminal clipped it to a single
        // letter — "Smart" rendered as "S" at 60 columns. Telemetry is what a
        // cramped line should lose, not the safety signal, so build it as
        // required + least-important-first optional segments.
        let required = vec![
            Span::styled(" $ ", sky),
            Span::styled(
                ctx.model.clone(),
                Style::default()
                    .fg(Color::Rgb(94, 184, 255))
                    .add_modifier(Modifier::BOLD),
            ),
        ];

        let mut autonomy: Vec<Span> = Vec::new();
        append_autonomy_segment(&mut autonomy, ctx.autonomy_preset, muted);

        // Declaration order is display order; the rank decides what a cramped
        // line gives up. Telemetry goes before the safety signal.
        let optional = vec![
            (
                2,
                vec![
                    Span::styled("  │  ", muted),
                    Span::styled(used_label, Style::default().fg(Color::Rgb(126, 226, 179))),
                ],
            ),
            (
                1,
                vec![
                    Span::styled("  │  ", muted),
                    Span::styled(format!("{} msgs", ctx.messages.len()), muted),
                ],
            ),
            (
                3,
                vec![Span::styled("  │  ", muted), Span::styled(age_label, muted)],
            ),
            (0, autonomy),
        ];

        Line::from(fit_segments(required, optional, area.width))
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
fn render_splash_lines(ctx: &TuiContext, area_width: u16, area_height: u16) -> Vec<Line<'static>> {
    // On a terminal too short to hold the full figlet+mascot banner ABOVE the
    // composer, the top of the banner is pushed into scrollback ("run
    // rantaiclaw and only the input box shows"). Below this height, render a
    // compact 4-line banner instead so the whole thing fits on screen. The
    // full banner is ~22 rows and the composer + status take ~5, so anything
    // under ~26 rows can't show both.
    const FULL_BANNER_MIN_HEIGHT: u16 = 26;
    if area_height < FULL_BANNER_MIN_HEIGHT {
        return render_splash_compact(ctx, area_width);
    }

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
            .clamp(MIN_RIGHT_WIDTH, MAX_RIGHT_WIDTH)
    } else {
        avail.clamp(20, MAX_RIGHT_WIDTH)
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
        .filter(|(_, configured, _, _)| *configured)
        .map(|(name, _, _, _)| name.clone())
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

/// Join `items` with `, ` into a single line that fits `width` columns,
/// replacing the overflow with a `+N` suffix. Used by the compact splash so
/// the channel/skill list stays on one row on a short terminal.
fn summarize_list(items: &[String], width: usize) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut shown = 0usize;
    for name in items {
        let sep = if out.is_empty() { "" } else { ", " };
        let candidate = out.chars().count() + sep.len() + name.chars().count();
        // Leave room for a possible `, +NN` overflow marker.
        if shown > 0 && candidate + 6 > width {
            break;
        }
        out.push_str(sep);
        out.push_str(name);
        shown += 1;
    }
    let remaining = items.len() - shown;
    if remaining > 0 {
        use std::fmt::Write as _;
        let _ = write!(out, ", +{remaining}");
    }
    out
}

/// A 4-line splash for terminals too short to hold the full figlet+mascot
/// banner above the composer. Same brand/channels/skills/help content, on one
/// row each, so nothing scrolls off on launch.
fn render_splash_compact(ctx: &TuiContext, area_width: u16) -> Vec<Line<'static>> {
    let muted = Color::Rgb(107, 114, 128);
    let sky = Color::Rgb(94, 184, 255);
    let gold = Color::Rgb(234, 179, 8);
    let width = (area_width as usize).max(20);

    let channels: Vec<String> = ctx
        .channels_summary
        .iter()
        .filter(|(_, configured, _, _)| *configured)
        .map(|(name, _, _, _)| name.clone())
        .collect();
    let skills: Vec<String> = ctx
        .available_skills
        .iter()
        .map(|s| s.name.clone())
        .collect();

    // Label column ("Channels  ") is 10 cols; leave the rest for the value.
    let value_width = width.saturating_sub(10);
    let (ch_text, ch_muted) = if channels.is_empty() {
        ("none — /setup channels".to_string(), true)
    } else {
        (summarize_list(&channels, value_width), false)
    };
    let (sk_text, sk_muted) = if skills.is_empty() {
        ("none — /setup skills".to_string(), true)
    } else {
        (summarize_list(&skills, value_width), false)
    };

    vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "Rantaiclaw",
                Style::default().fg(sky).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  v{}", env!("CARGO_PKG_VERSION")),
                Style::default().fg(muted),
            ),
        ]),
        Line::from(vec![
            Span::styled("Channels  ", Style::default().fg(gold)),
            Span::styled(
                ch_text,
                Style::default().fg(if ch_muted { muted } else { sky }),
            ),
        ]),
        Line::from(vec![
            Span::styled("Skills    ", Style::default().fg(gold)),
            Span::styled(
                sk_text,
                Style::default().fg(if sk_muted { muted } else { sky }),
            ),
        ]),
        Line::from(Span::styled(
            "Type a message or /help for commands.",
            Style::default().fg(muted),
        )),
        Line::from(""),
    ]
}

#[cfg(test)]
mod splash_compact_tests {
    use super::summarize_list;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn summarize_list_fits_or_overflows_with_a_count() {
        assert_eq!(summarize_list(&[], 40), "");
        // Everything fits → full list.
        assert_eq!(summarize_list(&v(&["a", "b", "c"]), 40), "a, b, c");
        // Too narrow → keep what fits, mark the rest as +N.
        let long = v(&[
            "summarizer",
            "scheduler-reminders",
            "research-assistant",
            "web-search",
            "meeting-notes",
        ]);
        let out = summarize_list(&long, 24);
        assert!(
            out.contains('+'),
            "expected a +N overflow marker, got {out:?}"
        );
        assert!(
            out.chars().count() <= 24 + 4,
            "compact list should stay near the width, got {out:?}"
        );
        // At least the first item always shows, even on an absurdly small width.
        assert!(summarize_list(&long, 3).starts_with("summarizer"));
    }
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
    use super::{compact_args_for_log, tool_status_line};

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

    /// The transcript's tool-status line cropped the first line of stdout by
    /// byte index, so an `ls` over a filename with an accented or CJK
    /// character panicked the render loop. Same class as the
    /// `compact_args_for_log` crop above, missed at this site.
    #[test]
    fn tool_status_line_crops_every_multibyte_width_safely() {
        for filler in ['é', '世', '🦀'] {
            for pad in 40..70 {
                let out = format!("{}{}", "a".repeat(pad), filler.to_string().repeat(20));
                for ok in [true, false] {
                    // Returning at all is the assertion: a byte crop panics.
                    let line = tool_status_line(&out, ok);
                    assert!(!line.is_empty(), "pad {pad} filler {filler} ok {ok}");
                }
            }
        }
    }

    #[test]
    fn tool_status_line_labels_empty_and_failed_output() {
        assert_eq!(tool_status_line("", true), "ok");
        assert_eq!(tool_status_line("", false), "error");
        assert_eq!(tool_status_line("done\nmore", true), "done");
        assert_eq!(tool_status_line("boom", false), "error: boom");
        assert!(tool_status_line(&"b".repeat(200), true).ends_with('…'));
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

/// The one-line status a finished tool call gets in the transcript: the first
/// line of its output, cropped.
fn tool_status_line(output_preview: &str, ok: bool) -> String {
    const MAX_LEN: usize = 60;
    let preview = output_preview.lines().next().unwrap_or("").trim();
    if preview.is_empty() {
        return if ok { "ok" } else { "error" }.to_string();
    }
    // Crop by chars, not bytes: this is tool stdout, so it carries filenames
    // and model text, and a byte crop panics the render loop when it lands
    // mid-codepoint — `ls` over an accented filename was enough.
    let cropped = super::render::truncate_preview(preview, MAX_LEN);
    if ok {
        cropped
    } else {
        format!("error: {cropped}")
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
/// Default inline viewport height — the size of an EMPTY composer: a 1-row
/// input box (top border + 1 text row + bottom border = 3) + 1 status row = 4.
/// The composer grows/shrinks from here via the dynamic-height reconcile, and
/// `composer_viewport_rows` is seeded with this value so the FIRST rendered
/// frame already matches (no startup `terminal.clear()` + recreate churn, which
/// on some terminals — notably Windows conhost — displaced the composer).
pub const INLINE_VIEWPORT_LINES: u16 = 4;
/// Retained constant (= 0) so old call sites that used it as an offset
/// keep type-checking. The stream-preview pane was removed in v0.6.50;
/// re-introducing it means flipping this to `1` and adding the
/// `Constraint::Length(STREAM_PREVIEW_LINES)` back to the layout.
pub const STREAM_PREVIEW_LINES: u16 = 0;

/// Set up the terminal in **fullscreen (alternate-screen) mode**.
///
/// The whole terminal is the app's canvas: `TuiApp::render` lays out a
/// scrollable chat pane on top, the composer, and the status bar — with the
/// composer pinned above the status bar at the very bottom on EVERY terminal
/// (including Windows conhost, where the old inline viewport floated to the
/// top). This matches how Claude Code works. On exit
/// the alternate screen is left and the original shell screen is restored.
pub fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    enter_fullscreen()
}

/// Enter the alternate screen, re-arm bracketed paste, and build a fullscreen
/// ratatui terminal. Used at startup and after returning from `$EDITOR` (which
/// leaves the alternate screen).
///
/// Bracketed paste is a terminal *mode* (`ESC[?2004h`) a screen switch can
/// clear, so it is re-armed on every (re)entry — multi-line pastes then arrive
/// as one `Event::Paste` rather than per-key `\n` = `Enter` events.
fn enter_fullscreen() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    // Mouse capture lets the wheel scroll the chat pane. Without it, terminals
    // in alt-screen translate the wheel into Up/Down arrows, which the composer
    // then treats as history recall — so scrolling silently rewrote the input.
    let _ = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    );
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;
    Ok(terminal)
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
    match edit_text_in_external_editor(terminal, &app.context.input_buffer, "prompt")? {
        EditorOutcome::Saved { mut text, path } => {
            if text.ends_with('\n') {
                text.pop();
            }
            app.context.input_buffer = text;
            app.context.cursor_to_end();
            app.context.exit_history_navigation();
            // Best-effort cleanup; file is in $TMPDIR so leftovers are harmless.
            let _ = std::fs::remove_file(&path);
            Ok(())
        }
        EditorOutcome::Failed { reason, path } => {
            app.context.last_error = Some(format!("{reason} — buffer unchanged"));
            let _ = std::fs::remove_file(&path);
            Ok(())
        }
    }
}

/// A deferred `/skill new` / `/skill edit`, raised by dispatch and performed by
/// `run_loop`.
pub struct SkillEditRequest {
    pub slug: String,
    pub path: PathBuf,
    pub initial: String,
    pub is_new: bool,
}

/// Maximum times the editor reopens on a validation failure before giving up
/// and reporting the staged path. Without a cap, someone who cannot get past
/// validation is trapped in a reopening editor.
const SKILL_EDIT_ATTEMPTS: usize = 3;

/// The manifest `name:` a body would load as, or `None` when the loader could
/// not read it.
fn manifest_name(content: &str) -> Option<String> {
    crate::skills::parse_yaml_frontmatter(content)
        .get("name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Why a saved body was rejected, phrased for the top of the reopened file.
fn validate_skill_body(content: &str, req: &SkillEditRequest) -> Result<(), String> {
    let Some(name) = manifest_name(content) else {
        return Err(
            "frontmatter is missing or has no `name:` — a skill without one never loads"
                .to_string(),
        );
    };

    if req.is_new {
        // Keep the directory and the manifest agreeing. The folder was staged
        // from the name typed at the prompt; renaming inside the editor would
        // leave a skill whose folder says one thing and whose manifest says
        // another.
        let slug = crate::tools::author_skill::slugify(&name);
        if slug != req.slug {
            return Err(format!(
                "name changed to `{name}`, which wants folder `{slug}` but this one is `{}` — \
                 run `/skill new \"{name}\"` instead",
                req.slug
            ));
        }
    } else {
        // The name is the `[skills.entries.<name>]` config key, so changing
        // even its case orphans the entry and silently resets whether the
        // skill is enabled.
        let current = manifest_name(&req.initial).unwrap_or_default();
        if name != current {
            return Err(format!(
                "renaming is not supported here: this skill is `{current}`, the file now says \
                 `{name}`"
            ));
        }
    }
    Ok(())
}

/// Hand a skill's `SKILL.md` to `$EDITOR`, validate what comes back, and commit
/// it. Returns the line to show the user.
///
/// On a validation failure the editor **reopens** with their text intact and
/// the reason as a comment at the top — the `git commit` pattern. A typo after
/// pasting 200 lines must not mean starting over, and this needs no `--resume`
/// flag or stashed-file bookkeeping to achieve.
fn run_skill_editor(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    req: &SkillEditRequest,
) -> Result<String> {
    let mut buffer = req.initial.clone();

    for attempt in 1..=SKILL_EDIT_ATTEMPTS {
        let outcome = edit_text_in_external_editor(terminal, &buffer, "skill")?;
        let (text, staged) = match outcome {
            EditorOutcome::Saved { text, path } => (text, path),
            EditorOutcome::Failed { reason, path } => {
                let _ = std::fs::remove_file(&path);
                return Ok(format!("{reason} — nothing was saved."));
            }
        };

        // Strip any comment banner a previous attempt prepended, so it never
        // accumulates or ends up in the committed file.
        let cleaned: String = text
            .lines()
            .skip_while(|l| l.starts_with("# ✗ ") || l.starts_with("#   "))
            .collect::<Vec<_>>()
            .join("\n");
        let cleaned = format!("{}\n", cleaned.trim_start_matches('\n').trim_end());

        match validate_skill_body(&cleaned, req) {
            Ok(()) => {
                let _ = std::fs::remove_file(&staged);
                return commit_skill_body(&cleaned, req);
            }
            Err(reason) if attempt < SKILL_EDIT_ATTEMPTS => {
                let _ = std::fs::remove_file(&staged);
                buffer = format!(
                    "# ✗ {reason}\n#   Fix it and save again, or quit without saving to cancel.\n{cleaned}"
                );
            }
            Err(reason) => {
                // Out of attempts: keep the staged file so the work survives.
                return Ok(format!(
                    "Not saved: {reason}. Your text is at {}",
                    staged.display()
                ));
            }
        }
    }
    unreachable!("loop returns on every path")
}

/// Write the validated body, plus an origin marker for a newly created skill.
fn commit_skill_body(content: &str, req: &SkillEditRequest) -> Result<String> {
    let dir = req
        .path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("skill path has no parent directory"))?;
    std::fs::create_dir_all(dir)?;

    // Stage then rename: a truncated SKILL.md still parses as *something*, and
    // that something becomes the agent's instructions on the next load.
    let staged = dir.join("SKILL.md.staged");
    std::fs::write(&staged, content)?;
    std::fs::rename(&staged, &req.path)?;

    if req.is_new {
        // Best-effort, like every other origin write.
        if let Err(e) = crate::skills::origin::write_origin(
            dir,
            &crate::skills::origin::SkillOrigin::new(
                crate::skills::origin::SkillOriginKind::Authored,
                None,
            ),
        ) {
            tracing::warn!("could not record origin for {}: {e}", req.slug);
        }
        Ok(format!("Created skill '{}'.", req.slug))
    } else {
        Ok(format!("Saved '{}'.", req.slug))
    }
}

/// What came back from the editor handoff.
///
/// The staged path is reported either way: on failure the caller decides
/// whether to keep it (a skill body the user just spent effort on) or discard
/// it (a composer buffer that is still in the composer).
enum EditorOutcome {
    Saved { text: String, path: PathBuf },
    Failed { reason: String, path: PathBuf },
}

/// Suspend the TUI, hand `initial` to the user's editor, and return whatever
/// they saved.
///
/// `file_stem` names the staged file so a stray leftover in `$TMPDIR` says what
/// it was. Always restores raw mode and the alternate screen before returning,
/// including on failure, so the caller can resume drawing unconditionally.
fn edit_text_in_external_editor(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    initial: &str,
    file_stem: &str,
) -> Result<EditorOutcome> {
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

    // Write the text to a temp file; the editor edits in place.
    // Use a unique filename in the OS temp dir (no extra dep needed).
    let pid = std::process::id();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_path = std::env::temp_dir().join(format!("rantaiclaw-{file_stem}-{pid}-{nonce}.md"));
    std::fs::write(&tmp_path, initial)?;

    // Suspend the TUI: leave the alternate screen, drop raw mode. `enter_fullscreen`
    // re-enables mouse capture + bracketed paste on return.
    let _ = terminal.flush();
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    disable_raw_mode()?;
    let _ = io::stdout().flush();

    // Run the editor. Inherit stdio so the user sees / interacts with it.
    let mut parts = editor_cmd.split_whitespace();
    let bin = parts.next().unwrap_or("vi");
    let args: Vec<&str> = parts.collect();
    let status = Command::new(bin).args(&args).arg(&tmp_path).status();

    // Restore raw mode + re-enter the alternate screen with a fresh terminal.
    enable_raw_mode()?;
    *terminal = enter_fullscreen()?;

    Ok(match status {
        Ok(s) if s.success() => {
            let mut buf = String::new();
            std::fs::File::open(&tmp_path).and_then(|mut f| f.read_to_string(&mut buf))?;
            EditorOutcome::Saved {
                text: buf,
                path: tmp_path,
            }
        }
        Ok(s) => EditorOutcome::Failed {
            reason: format!("editor exited with status {}", s.code().unwrap_or(-1)),
            path: tmp_path,
        },
        Err(e) => EditorOutcome::Failed {
            reason: format!("editor '{bin}' failed to launch: {e}"),
            path: tmp_path,
        },
    })
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

/// Restore the terminal to its original state on exit: leave the alternate
/// screen (which restores the shell's pre-launch screen), disarm bracketed
/// paste, show the cursor, and drop raw mode.
pub fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let _ = terminal.show_cursor();
    restore_terminal_modes();
    Ok(())
}

/// Put the terminal back the way it was found, without needing a `Terminal`
/// handle — the panic hook has no access to one. Every step is best-effort and
/// idempotent, so calling this after [`restore_terminal`] (or twice) is safe.
fn restore_terminal_modes() {
    let _ = execute!(
        io::stdout(),
        crossterm::cursor::Show,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let _ = io::stdout().flush();
}

/// Chain a panic hook that restores the terminal before the default hook
/// prints the message.
///
/// Without it a panic anywhere in the render loop left raw mode, the alternate
/// screen and mouse capture switched on: the panic message was invisible and
/// the user's shell unusable until they blindly typed `reset`. Install it after
/// terminal setup, so a panic before that still prints normally.
fn install_terminal_restoring_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal_modes();
        previous(info);
    }));
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
/// A comparable fingerprint of the channel configuration.
///
/// The change detector used to compare channel **counts**, which misses every
/// count-neutral edit: rotating a leaked bot token, or swapping one channel for
/// another. A stale token is the security-relevant case — the listener keeps
/// polling with the credential the operator just revoked, and nothing is even
/// printed.
///
/// Serialised rather than `PartialEq`-derived because the config types are a
/// public contract and adding derives across them is a wider change than this
/// needs. A serialisation failure yields `None`, and the caller treats
/// `None != None` as "cannot prove it is unchanged", which errs toward
/// restarting.
pub(crate) fn channels_fingerprint(c: &crate::config::Config) -> Option<String> {
    // Include the autonomy policy alongside the channel config so an autonomy
    // change (e.g. `/autonomy strict`, `/allow …`) also restarts running channel
    // listeners with the new policy. A channels-only fingerprint left listeners
    // on the old, looser autonomy until an independent restart — a tightening the
    // operator saw on-screen but that wasn't enforced fleet-wide.
    serde_json::to_string(&(&c.channels_config, &c.autonomy)).ok()
}

/// Whether the channel configuration differs between two fingerprints.
///
/// Unprovable equality counts as a difference: a restart costs a brief
/// reconnect, a missed restart costs the whole session.
pub(crate) fn channels_differ(before: Option<&String>, after: Option<&String>) -> bool {
    match (before, after) {
        (Some(a), Some(b)) => a != b,
        _ => true,
    }
}

/// What a setup save changed, and therefore what the app may claim about it.
///
/// A save answers two questions that one fingerprint used to conflate: does a
/// listener restart need to happen (yes for a channel change AND for a policy
/// change), and were channels actually configured (only for a channel change).
/// Answering the second with the first is how a wizard that configured no
/// channel at all printed "✓ Channels configured".
///
/// The variants are ordered by how much they claim, so `max` keeps the
/// strongest of two changes: `None` < `PolicyOnly` < `Channels`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum SetupChange {
    /// Nothing a listener cares about changed.
    #[default]
    None,
    /// Only the policy changed: the listeners restart, but no channel was
    /// configured.
    PolicyOnly,
    /// The channel configuration changed: the listeners restart and channels
    /// were configured.
    Channels,
}

/// A comparable fingerprint of the channel configuration alone.
///
/// The restart fingerprint above also covers the autonomy policy, because a
/// policy change must restart the listeners too. That makes it the wrong
/// instrument for the other question — "were channels actually configured?" —
/// which this one answers on its own.
pub(crate) fn channels_only_fingerprint(c: &crate::config::Config) -> Option<String> {
    serde_json::to_string(&c.channels_config).ok()
}

/// Whether the channel configuration itself changed.
///
/// Unlike the restart fingerprint, an unprovable comparison counts as
/// *unchanged* here: this answer decides what the app may claim about what it
/// did, and a claim it cannot prove is a lie.
fn channels_config_changed(before: &crate::config::Config, after: &crate::config::Config) -> bool {
    match (
        channels_only_fingerprint(before),
        channels_only_fingerprint(after),
    ) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    }
}

/// What a save changed, in the two terms the app needs.
///
/// The restart is owed for anything the restart fingerprint sees — a channel
/// change or a policy change. The channels claim is made only for a channel
/// change.
pub(crate) fn setup_change(
    before: &crate::config::Config,
    after: &crate::config::Config,
) -> SetupChange {
    let before_fp = channels_fingerprint(before);
    let after_fp = channels_fingerprint(after);
    if !channels_differ(before_fp.as_ref(), after_fp.as_ref()) {
        return SetupChange::None;
    }
    if channels_config_changed(before, after) {
        SetupChange::Channels
    } else {
        SetupChange::PolicyOnly
    }
}

/// The status-bar label for a config: `provider:model`, the spelling the boot
/// path (`boot_model_label`) and the wizard save store build. A config that
/// names no provider labels its model alone; a config that names no model at
/// all has no label, and the caller leaves the status bar as it is.
fn model_label_of(config: &crate::config::Config) -> String {
    let provider = config.default_provider.as_deref().unwrap_or_default();
    let model = config.default_model.as_deref().unwrap_or_default();
    match (provider.is_empty(), model.is_empty()) {
        (false, false) => format!("{provider}:{model}"),
        (true, false) => model.to_string(),
        _ => String::new(),
    }
}

/// Whether a session's recorded model names something other than the model the
/// running agent is on.
///
/// A session records the label as it stood when it opened, so what it holds is
/// usually the `provider:model` the status bar shows. The CLI (`agent -m`) and
/// the API (`record_api_turn`) record a bare model id instead, which that label
/// spells with its provider in front — both spellings name the same model then,
/// and only a real difference is worth telling the operator about. A row with
/// no model recorded names nothing, so it is not a difference either; neither is
/// one recorded against a running label that names no model, which would put an
/// empty label in the middle of the note.
fn session_model_differs(recorded: &str, running: &str) -> bool {
    let recorded = recorded.trim();
    if recorded.is_empty() || running.trim().is_empty() || recorded == running {
        return false;
    }
    !running
        .strip_suffix(recorded)
        .is_some_and(|provider| provider.ends_with(':'))
}

/// Whether the agent can be built from `config`, judged by the same provider
/// factory and argument order `Agent::from_config_with_observer` uses
/// (`src/agent/agent.rs`). Construction only: no request is sent to the
/// provider. The qwen and MiniMax OAuth resolution this factory performs runs
/// on every agent build already (launch and every reload), so this check adds
/// no network request class.
pub(crate) fn check_model_buildable(config: &crate::config::Config) -> anyhow::Result<()> {
    let provider_name = config.default_provider.as_deref().unwrap_or("openrouter");
    let model_name = config
        .default_model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| anyhow::anyhow!("no model is set"))?;
    let provider_credential = config.resolve_key_for_provider(provider_name);
    crate::providers::create_routed_provider(
        provider_name,
        provider_credential.as_deref(),
        config.api_url.as_deref(),
        &config.reliability,
        &config.model_routes,
        model_name,
        &|name| config.resolve_key_for_provider(name),
    )
    .map(|_| ())
    .map_err(|e| anyhow::anyhow!("{provider_name} cannot run this model: {e:#}"))
}

/// Whether the active provider can actually send a message: it has a key
/// resolved the way the send paths resolve it, or it is a local provider that
/// needs none. `None` when there is no `default_provider` to judge.
///
/// Mirrors the `config.provider_key` doctor check so the TUI panel and the CLI
/// doctor agree. The panel's old `!ctx.model.is_empty()` was structurally
/// unfalsifiable — `Config::default` always sets a model — so it reported
/// "✓ Model configured" on a keyless install.
pub(crate) fn provider_key_status(c: &crate::config::Config) -> Option<bool> {
    let provider = c.default_provider.as_deref()?;
    Some(
        crate::providers::provider_is_local(provider)
            || c.resolve_key_for_provider(provider).is_some(),
    )
}

/// Re-feeds the stored turns of session `resume_id` to `agent`, so the model
/// remembers the earlier conversation.
async fn restore_resumed_session(agent: &mut Agent, resume_id: &str) {
    match crate::sessions::cli::open_store().and_then(|s| s.get_messages(resume_id)) {
        Ok(msgs) => {
            let prior = crate::sessions::turns_for_history(&msgs);
            if !prior.is_empty() {
                // Rebuilding the system prompt reads the view, and the actor
                // sets it only around each turn. The TUI is the operator's own
                // surface, so the rebuild runs under the `All` view too.
                let restored = crate::memory::MEMORY_VIEW
                    .scope(
                        crate::memory::MemoryView::All,
                        agent.restore_history(&prior),
                    )
                    .await;
                if let Err(e) = restored {
                    tracing::warn!("failed to restore resumed history: {e}");
                }
            }
        }
        Err(e) => tracing::warn!("could not load resumed session {resume_id}: {e}"),
    }
}

/// Which door the boot opens when no explicit setup topic was given.
#[derive(Debug, PartialEq, Eq)]
enum BootDoor {
    /// Nothing opens: the profile is set up and its agent booted.
    None,
    /// The first-run wizard: the profile has no provider credential yet.
    FirstRunWizard,
    /// The provider overlay: the profile was set up and its provider broke.
    ProviderOverlay,
}

/// Decide which door the TUI boot opens when no explicit `setup <topic>` was
/// given.
///
/// The agent's own build result is the FIRST question, because it is the
/// only one that knows whether this install can talk to a provider at all:
///
/// - The agent built: this install can send, so there is nothing to set up
///   and no door opens. This must be checked FIRST — an install whose
///   credential lives in the environment, or whose provider needs no
///   credential at all (`ollama`, `llamacpp`), stores no key in
///   `config.toml`, but it is not a fresh install, and opening the wizard
///   over it would be a regression.
/// - The agent did not build and this config holds a provider credential:
///   the profile WAS set up and something about the provider broke. The
///   provider overlay owns exactly the keys (`default_provider` / `api_key` /
///   `default_model`) whose current values broke the boot.
/// - The agent did not build and no credential is stored in this config: the
///   profile was never set up, so the first-run wizard is the door that sets
///   it up.
///
/// A genuinely fresh install always lands in the last case: `Config::default`
/// sets no model and `Agent::build` refuses to guess one, so the agent does
/// not build and the wizard is reachable.
///
/// The credential question itself is asked by
/// [`crate::config::Config::provider_credential_resolves`] — the same rule
/// the onboarding provider section asks, so the two surfaces cannot drift
/// apart on it.
fn boot_door(config: &crate::config::Config, agent_boot_error: Option<&str>) -> BootDoor {
    if agent_boot_error.is_none() {
        return BootDoor::None;
    }
    if config.provider_credential_resolves() {
        return BootDoor::ProviderOverlay;
    }
    BootDoor::FirstRunWizard
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

    // A provider that cannot construct (e.g. `openai` saved with no API key)
    // used to abort right here — which also killed `rantaiclaw setup`, the
    // documented repair path, leaving no way back in short of hand-editing
    // config.toml. Degrade instead: boot without an agent, surface the error,
    // open the provider setup overlay, and let TurnRequest::Reload heal the
    // session in place once a working config is saved.
    let (mut agent, agent_boot_error) = match Agent::from_config(&app_config).await {
        Ok(mut agent) => {
            agent.set_surface("tui");
            (Some(agent), None)
        }
        Err(e) => {
            tracing::error!("agent failed to start: {e:#}");
            (None, Some(format!("{e:#}")))
        }
    };

    // `/resume`: re-feed the resumed session's prior turns so the model
    // actually remembers the earlier conversation (not just the scrollback).
    if let Some(resume_id) = tui_config.resume_session.as_deref() {
        if let Some(agent) = agent.as_mut() {
            restore_resumed_session(agent, resume_id).await;
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

    let security_handle = agent.as_ref().and_then(|a| a.security());
    let memory_handle = agent.as_ref().map(|a| a.memory_handle());
    let mcp_tools_by_server = agent
        .as_ref()
        .map(|a| a.mcp_tools_by_server())
        .unwrap_or_default();
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
    // The actor answers a `Submit` it cannot run with the no-agent error and
    // no `Done`, so the TUI has to know when that is the case: the boot
    // result is that fact, and a reload heals it in place.
    app.actor_has_agent = agent_boot_error.is_none();
    app.context.workspace_dir = Some(app_config.workspace_dir.clone());
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
    app.context.memory = memory_handle;
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

    // Boot-time provider failure: tell the operator what broke and how the
    // session heals. The overlay itself opens below — an explicit
    // `setup <topic>` still wins; otherwise we route to the provider
    // provisioner, the one that repairs this state.
    if let Some(err) = agent_boot_error.as_deref() {
        let msg = format!(
            "⚠ provider failed to start: {err}. Opening setup — enter an API key or \
             switch provider; the session heals in place once saved."
        );
        let _ = app.context.append_system_message(&msg);
        app.scrollback_queue.push(("system".to_string(), msg));
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
    } else {
        // No explicit setup topic: the boot decides which door opens.
        // - The agent built: this install can send, so nothing opens. An
        //   install whose credential lives in the environment, or whose
        //   provider needs none (ollama, llamacpp), is in this case: it
        //   holds no key in config.toml, but it is not a fresh install, and
        //   the wizard over it would be a regression.
        // - The agent did not build and this config holds a credential: the
        //   profile WAS set up and the provider broke, so the provider
        //   overlay opens — it owns exactly the keys (default_provider /
        //   api_key / default_model) whose current values broke the boot.
        // - The agent did not build and no credential is stored here: the
        //   profile was never set up, so the first-run wizard opens — it is
        //   the door that sets a credential up.
        match boot_door(&app_config, agent_boot_error.as_deref()) {
            BootDoor::FirstRunWizard => {
                app.first_run_wizard = Some(crate::tui::FirstRunWizard::new(profile.clone()));
            }
            BootDoor::ProviderOverlay => {
                if let Err(e) = app.open_setup_overlay("provider".to_string()) {
                    let msg = format!("Failed to open provider setup: {e}");
                    let _ = app.context.append_system_message(&msg);
                    app.scrollback_queue.push(("system".to_string(), msg));
                }
            }
            BootDoor::None => {}
        }
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
    let configured_channels = crate::channels::configured_channel_count(&app_config);
    app.context.provider_key_ok = provider_key_status(&app_config);
    app.context.approval_boundary = (
        app_config.channels_config.approval_owners.len(),
        app_config.channels_config.autonomous_tools,
    );
    app.context.channels_summary = crate::channels::channel_status_roster(&app_config)
        .into_iter()
        .map(|(name, configured, support, verification)| {
            (name.to_string(), configured, support, verification)
        })
        .collect();
    app.context.running_whatsapp_surface = app_config.channels_config.running_whatsapp_surface();
    if configured_channels > 0 {
        // Spawn the channel runtime as a cancellable supervisor (stored on
        // `app`) rather than a fire-and-forget task, so a mid-session
        // `/setup <channel>` or skill add can restart it in place via
        // `restart_channels`. `app.config` is `app_config.clone()` (see the
        // `TuiApp::new` call above), so this uses the same decrypted config.
        app.restart_channels();
    }

    let mut terminal = setup_terminal()?;
    install_terminal_restoring_panic_hook();

    // The splash banner is rendered by `render_chat_pane` as the empty-chat
    // state — no separate scrollback commit needed in fullscreen mode.
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
    // Auto-lock clock. Local to the loop on purpose: the gate is re-armed from
    // right here and nothing else needs to read it, so this stays off `App` and
    // out of its five test fixtures. Any terminal event counts as operator
    // presence (keys, mouse, resize, focus).
    let mut last_input_at = std::time::Instant::now();

    loop {
        // Drain any buffered agent events before rendering so the frame
        // reflects the latest streaming state.
        app.drain_events();

        // Re-arm the login gate after an idle stretch, so an unattended
        // terminal does not stay unlocked. Config is re-read every iteration
        // rather than captured once, so a reload that changes the timeout (or
        // removes the credential entirely) takes effect without a restart.
        // The gate masks the UI only — an in-flight turn keeps streaming behind
        // it and is intact when the operator unlocks.
        if crate::tui::widgets::login_gate::should_relock(
            app.login_gate.is_some(),
            app.config.gateway.login.password_hash.is_some(),
            std::time::Duration::from_secs(app.config.gateway.login.idle_timeout_secs),
            last_input_at.elapsed(),
        ) {
            app.login_gate = Some(crate::tui::LoginGateState::new(
                app.config.gateway.login.username.clone(),
            ));
        }

        // /new and /clear wipe the screen; the next render redraws a fresh
        // chat pane from the (now-empty) message list.
        if app.clear_terminal_request {
            app.clear_terminal_request = false;
            let _ = terminal.clear();
        }

        // One fullscreen frame: the scrollable chat pane, the composer, the
        // status bar, and any active modal (login gate, wizard, setup overlay,
        // list picker, info panel, `/help`, autocomplete dropdown) drawn
        // over/instead of the base — all handled inside `render`.
        app.render(terminal)?;

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
            // Operator is present — push the auto-lock deadline out. Done for
            // every event kind, including the ones consumed by the gate itself,
            // so typing a password can never race the clock that armed it.
            last_input_at = std::time::Instant::now();
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
                let _ = terminal.clear();
            } else if !app.modal_active()
                && app.overlay.is_none()
                && app.pending_approval.is_none()
                && leading_text_key(&ev).is_some()
            {
                // Potential paste burst: a printable key with more input
                // already queued. Drain the run of text keys into one batch;
                // a batch that is multi-char or multi-line is a paste, so
                // internal Enters become '\n' instead of submitting. A lone
                // char (fast single keystroke) falls through to normal typing.
                //
                // Gated to when the chat composer actually owns the input:
                // no full-screen modal (`alt` covers login gate, list
                // picker, info panel, autocomplete dropdown, setup overlay,
                // first-run wizard — see `want_alt` above), no `/help`
                // overlay, and no pending-approval prompt. Those surfaces
                // have their own key handling in `handle_key` (Tab-complete,
                // picker install, overlay dismiss, Y/N/A approval) that a
                // blind paste-coalescing intercept would otherwise shadow.
                let first =
                    leading_text_key(&ev).expect("guarded by leading_text_key(&ev).is_some()");
                let mut batch = String::new();
                push_text_key(&mut batch, first);
                let mut trailing: Option<Event> = None;
                while event::poll(std::time::Duration::from_millis(0))? {
                    let next = event::read()?;
                    match leading_or_inner_text_key(&next) {
                        Some(tk) => push_text_key(&mut batch, tk),
                        None => {
                            trailing = Some(next);
                            break;
                        }
                    }
                }
                if batch_is_paste(&batch) {
                    app.apply_paste_text(&batch);
                } else {
                    // Single char: normal typing path.
                    if let Some(c) = batch.chars().next() {
                        app.context.insert_char_at_cursor(c);
                        app.context.exit_history_navigation();
                        app.refresh_autocomplete();
                    }
                }
                if let Some(other) = trailing {
                    match app.handle_event(other).await? {
                        EventResult::Quit => break,
                        EventResult::Continue => {}
                    }
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

        if let Some(req) = app.skill_editor_request.take() {
            match run_skill_editor(terminal, &req) {
                Ok(msg) => {
                    let _ = app.context.append_system_message(&msg);
                    app.scrollback_queue.push(("system".to_string(), msg));
                    app.refresh_available_skills();
                }
                Err(e) => {
                    app.context.last_error = Some(format!("skill editor error: {e}"));
                }
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

    fn clawhub_skill(slug: &str, owner: &str) -> crate::skills::clawhub::ClawHubSkill {
        crate::skills::clawhub::ClawHubSkill {
            slug: slug.into(),
            display_name: "Weather".into(),
            summary: "Get current weather and forecasts.".into(),
            owner_handle: owner.into(),
            official: false,
            stats: crate::skills::clawhub::ClawHubStats::default(),
            tags: serde_json::Value::Null,
        }
    }

    #[test]
    fn clawhub_result_row_keys_by_publisher_reference() {
        // The key is fed straight to `install_one`, so a search result whose
        // publisher is known must install that publisher's copy — not
        // whichever one a bare slug happens to resolve to.
        let item = clawhub_result_item(
            &clawhub_skill("weather", "steipete"),
            &std::collections::HashMap::new(),
        );
        assert_eq!(item.key, "@steipete/weather");

        // The listing endpoint reports no publisher at all; those rows stay
        // bare and resolve through the ambiguity prompt instead.
        let unowned = clawhub_result_item(
            &clawhub_skill("weather", ""),
            &std::collections::HashMap::new(),
        );
        assert_eq!(unowned.key, "weather");
    }

    #[test]
    fn clawhub_result_row_shows_publisher_and_popularity() {
        let mut skill = clawhub_skill("weather", "steipete");
        skill.official = true;
        skill.stats.downloads = 165_212;
        let item = clawhub_result_item(&skill, &std::collections::HashMap::new());
        // Two same-slug rows differ only by publisher, so the handle has to
        // be on screen for the choice to mean anything.
        assert!(item.primary.contains("@steipete"), "{}", item.primary);
        assert!(item.primary.contains('✓'), "{}", item.primary);
        assert!(item.primary.contains("165212"), "{}", item.primary);

        // Listings carry stars instead; showing a 0 for the absent metric
        // would read as "nobody uses this".
        let mut starred = clawhub_skill("weather", "");
        starred.stats.stars = 427;
        let listed = clawhub_result_item(&starred, &std::collections::HashMap::new());
        assert!(listed.primary.contains("★427"), "{}", listed.primary);
        assert!(!listed.primary.contains('↓'), "{}", listed.primary);
    }

    #[test]
    fn clawhub_candidate_rows_are_fully_qualified() {
        let ambiguous = crate::skills::clawhub::AmbiguousSkill {
            slug: "weather".into(),
            matches: vec![
                crate::skills::clawhub::AmbiguousMatch {
                    owner_handle: "steipete".into(),
                    reference: "@steipete/weather".into(),
                    url: "https://clawhub.ai/steipete/skills/weather".into(),
                    downloads: 165_212,
                    official: true,
                },
                crate::skills::clawhub::AmbiguousMatch {
                    owner_handle: "lfengwa2".into(),
                    reference: "@lfengwa2/weather".into(),
                    url: String::new(),
                    downloads: 57,
                    official: false,
                },
            ],
        };
        let items = clawhub_candidate_items(&ambiguous);
        // Every candidate key must be qualified: selecting one has to
        // install that publisher, not re-enter the same ambiguity.
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key, "@steipete/weather");
        assert_eq!(items[1].key, "@lfengwa2/weather");
        assert!(items.iter().all(|i| i.key.starts_with('@')));

        // The row has to say something about the publisher, not just name it.
        // Choosing between bare handles is choosing blind, and this is the one
        // decision the flow deliberately refuses to make for the user.
        assert_eq!(items[0].primary, "@steipete  (165212 installs · official)");
        assert_eq!(items[1].primary, "@lfengwa2  (57 installs)");
    }

    #[test]
    fn clawhub_candidate_row_falls_back_to_a_bare_handle_when_nothing_is_known() {
        // Enrichment is best-effort — the lookup that fills in install counts
        // can fail. A bare handle is right then; "0 installs" would read as
        // "nobody uses this", which is a claim we have not earned.
        let ambiguous = crate::skills::clawhub::AmbiguousSkill {
            slug: "weather".into(),
            matches: vec![crate::skills::clawhub::AmbiguousMatch {
                owner_handle: "steipete".into(),
                reference: "@steipete/weather".into(),
                url: String::new(),
                downloads: 0,
                official: false,
            }],
        };
        let items = clawhub_candidate_items(&ambiguous);
        assert_eq!(items[0].primary, "@steipete");
    }

    #[test]
    fn install_reference_and_local_slug_are_distinct() {
        // The picker installs by reference but preselects the freshly
        // installed skill in the Skill picker, whose rows are keyed by local
        // manifest name. Reusing the reference there would silently fail to
        // match, so the slug is derived separately.
        let reference = clawhub_result_item(
            &clawhub_skill("weather", "steipete"),
            &std::collections::HashMap::new(),
        )
        .key;
        let slug = crate::skills::clawhub::parse_skill_ref(&reference)
            .map_or_else(|_| reference.clone(), |parsed| parsed.slug);
        assert_eq!(reference, "@steipete/weather");
        assert_eq!(slug, "weather");
    }

    /// THE plan's primary test. The save handler swaps the new config into
    /// `self.config` immediately (so the next provisioner's clone sees this
    /// provisioner's writes), and the detector then read "previous" from that
    /// already-swapped value — previous always equalled new, so the restart
    /// never fired and `/setup channels` left the runtime untouched for the
    /// rest of the session.
    #[test]
    fn a_saved_channel_change_is_detected_before_the_swap() {
        let before = channels_fingerprint(&crate::config::Config::default());
        let after = channels_fingerprint(&cfg_with_telegram("123:XYZ"));
        assert!(
            channels_differ(before.as_ref(), after.as_ref()),
            "adding a channel must be detected"
        );

        // The bug's shape: comparing the swapped config against itself.
        assert!(
            !channels_differ(after.as_ref(), after.as_ref()),
            "the same config must not look changed"
        );
    }

    /// Count comparison missed this entirely: rotating a leaked token is
    /// count-neutral, so the listener kept polling with the credential the
    /// operator had just revoked — and nothing was printed.
    #[test]
    fn token_rotation_is_detected() {
        let before = channels_fingerprint(&cfg_with_telegram("123:OLD"));
        let after = channels_fingerprint(&cfg_with_telegram("123:NEW"));
        assert!(channels_differ(before.as_ref(), after.as_ref()));
        assert_eq!(
            crate::channels::configured_channel_count(&cfg_with_telegram("123:OLD")),
            crate::channels::configured_channel_count(&cfg_with_telegram("123:NEW")),
            "precondition: the counts are equal, which is why counting missed it"
        );
    }

    #[test]
    fn autonomy_tightening_changes_the_restart_fingerprint() {
        // A tightening (e.g. /autonomy strict) must restart running listeners so
        // they enforce the new policy — the fingerprint now covers autonomy.
        let mut cfg = crate::config::Config::default();
        let before = channels_fingerprint(&cfg);
        cfg.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        let after = channels_fingerprint(&cfg);
        assert!(
            channels_differ(before.as_ref(), after.as_ref()),
            "an autonomy change must change the restart fingerprint"
        );
    }

    #[test]
    fn no_change_is_not_detected_as_a_change() {
        let cfg = cfg_with_telegram("123:XYZ");
        let a = channels_fingerprint(&cfg);
        let b = channels_fingerprint(&cfg);
        assert!(
            !channels_differ(a.as_ref(), b.as_ref()),
            "restart_channels re-binds every listener; firing it on a no-op save \
             costs a Telegram 409 window"
        );
    }

    // ── What a setup save changed, and what the app says about it ──
    //
    // The restart fingerprint covers the autonomy policy as well as the
    // channel configuration, because a policy change must restart the
    // listeners too. That made it the wrong instrument for the other
    // question — "were channels actually configured?" — and the wizard
    // answered it with the policy's answer: a run that configured no
    // channel at all printed "✓ Channels configured".

    /// A `TuiApp` whose first-run wizard has reached its last phase.
    fn app_with_wizard_at_end(ctx: TuiContext, dir: &std::path::Path) -> TuiApp {
        let mut app = super::test_support::app_with_profile_root(ctx, dir.to_path_buf());
        let mut wizard = crate::tui::FirstRunWizard::new(crate::profile::Profile {
            name: "default".to_string(),
            root: dir.to_path_buf(),
        });
        wizard.phase = crate::tui::first_run_wizard::WizardPhase::Complete;
        app.first_run_wizard = Some(wizard);
        app
    }

    /// Write a config to the profile's `config.toml`, the way a provisioner's
    /// save lands on disk.
    fn write_config(dir: &std::path::Path, cfg: &crate::config::Config) {
        let text = toml::to_string_pretty(cfg).expect("serialize the config");
        std::fs::write(dir.join("config.toml"), text).expect("write the config");
    }

    /// Hand the app the config a provisioner just saved, the way the
    /// save-complete branch sees it.
    fn deliver_saved_config(app: &mut TuiApp, saved: crate::config::Config) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tx.send(saved).expect("send the saved config");
        app.setup_save_complete_rx = Some(rx);
        app.drain_events();
    }

    /// The lines the app said, in order.
    fn said(app: &TuiApp) -> Vec<String> {
        app.scrollback_queue
            .iter()
            .map(|(_, m)| m.clone())
            .collect()
    }

    /// The audit drive caught this: the wizard ran the approvals provisioner,
    /// which writes the autonomy level, and the wizard then said it had
    /// configured channels. No channel was chosen.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_wizard_does_not_claim_channels_when_it_saved_only_a_policy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = app_with_wizard_at_end(ctx, dir.path());

        // The approvals provisioner saved a policy; no channel was configured.
        let mut saved = app.config.clone();
        saved.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        write_config(dir.path(), &saved);
        deliver_saved_config(&mut app, saved);

        // The operator presses Enter to leave the wizard.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the wizard closes");

        let lines = said(&app);
        assert!(
            !lines.iter().any(|l| l.contains("Channels configured")),
            "the wizard configured no channel; it must not claim one: {lines:?}"
        );
    }

    /// The other half of the contract: a wizard that DID configure a channel
    /// still says so.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_wizard_claims_channels_when_it_saved_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = app_with_wizard_at_end(ctx, dir.path());

        // A channel provisioner saved its section. The section carries no
        // credential, so the restart starts no listener and the test reaches no
        // network — what is under test is the claim, not the listener.
        let mut saved = app.config.clone();
        saved.channels_config.telegram = Some(
            serde_json::from_value(serde_json::json!({
                "bot_token": "",
                "allowed_users": [],
            }))
            .expect("the telegram section"),
        );
        write_config(dir.path(), &saved);
        deliver_saved_config(&mut app, saved);

        // The operator presses Enter to leave the wizard.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .expect("the wizard closes");

        let lines = said(&app);
        assert!(
            lines.iter().any(|l| l.contains("Channels configured")),
            "the wizard configured a channel; it must say so: {lines:?}"
        );
    }

    /// A policy change still restarts the listeners: they have to run under
    /// the new policy, and a policy the listeners ignore is a policy the
    /// operator saw on screen but that was not enforced.
    ///
    /// The restart is asserted through the supervisor it replaces: the app
    /// held one, and `restart_channels` takes the supervisor it replaces, so
    /// an app that held one and holds none afterwards restarted. The
    /// supervisor carries no channel here, so no listener is started and the
    /// test reaches no network.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_policy_only_change_still_restarts_the_listeners() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = super::test_support::app_with_profile_root(ctx, dir.path().to_path_buf());

        // A listener is running.
        app.channel_supervisor = Some(ChannelSupervisor {
            shutdown: tokio_util::sync::CancellationToken::new(),
            handle: tokio::spawn(async {}),
        });

        // A save that moved the policy and nothing else.
        let mut saved = app.config.clone();
        saved.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        write_config(dir.path(), &saved);
        deliver_saved_config(&mut app, saved);

        assert!(
            app.channel_supervisor.is_none(),
            "a policy change must restart the listeners: they have to run under the new policy"
        );
    }

    /// The same save outside the wizard: the line names the policy, not the
    /// channel settings it never saved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_policy_only_save_does_not_claim_channel_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = super::test_support::app_with_profile_root(ctx, dir.path().to_path_buf());

        let mut saved = app.config.clone();
        saved.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        write_config(dir.path(), &saved);
        deliver_saved_config(&mut app, saved);

        let lines = said(&app);
        assert!(
            !lines.iter().any(|l| l.contains("Channel settings saved")),
            "no channel settings were saved; the line must not say they were: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("policy")),
            "the line must still name what did change: {lines:?}"
        );
    }

    /// The third case, and the one that keeps the other two honest: a save
    /// that changed neither the channels nor the policy has nothing to
    /// report. Both lines name a change, so a save with no change must
    /// print no line at all — otherwise the policy line can appear for a
    /// run that never touched the policy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_save_that_changed_nothing_prints_no_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = super::test_support::app_with_profile_root(ctx, dir.path().to_path_buf());

        // The same config, saved again: a setup run that kept everything.
        let saved = app.config.clone();
        write_config(dir.path(), &saved);
        deliver_saved_config(&mut app, saved);

        let lines = said(&app);
        assert!(
            lines.is_empty(),
            "a save that changed neither the channels nor the policy must \
             print no line at all: {lines:?}"
        );
    }

    /// The two fingerprints answer the two questions separately: the restart
    /// one moves for a policy change, the channels one does not.
    #[test]
    fn a_policy_change_moves_the_restart_fingerprint_but_not_the_channels_one() {
        let base = crate::config::Config::default();
        let mut policy = base.clone();
        policy.autonomy.level = crate::security::AutonomyLevel::ReadOnly;

        assert_eq!(
            channels_only_fingerprint(&base),
            channels_only_fingerprint(&policy),
            "the channels fingerprint must not move when only the policy moved"
        );
        assert!(
            channels_differ(
                channels_fingerprint(&base).as_ref(),
                channels_fingerprint(&policy).as_ref()
            ),
            "the restart fingerprint must still move: the listeners run under the new policy"
        );
    }

    /// And the change itself is classified the way the messages read it.
    #[test]
    fn setup_change_separates_a_policy_change_from_a_channel_change() {
        let base = crate::config::Config::default();
        assert_eq!(setup_change(&base, &base), SetupChange::None);

        let mut policy = base.clone();
        policy.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        assert_eq!(setup_change(&base, &policy), SetupChange::PolicyOnly);

        let mut channel = base.clone();
        channel.channels_config.telegram = cfg_with_telegram("123:XYZ").channels_config.telegram;
        assert_eq!(setup_change(&base, &channel), SetupChange::Channels);
    }

    /// QQ appeared nowhere in this file: configuring it left the count at zero,
    /// the restart returned early, and `/channels` listed it in neither
    /// section.
    #[test]
    fn roster_includes_qq_and_matches_the_shared_catalog() {
        let rows = crate::channels::channel_status_roster(&crate::config::Config::default());
        assert!(
            rows.iter().any(|(name, _, _, _)| *name == "QQ"),
            "QQ must appear in the roster: {rows:?}"
        );
        assert_eq!(
            rows.len(),
            crate::channels::channel_roster(&crate::config::Config::default()).len(),
            "the status roster and the configured roster are derived from one catalog"
        );
    }

    /// The two former TUI copies gated Matrix and Lark on build features while
    /// the provisioner registry offered them unconditionally, so a channel
    /// could be offered and then never displayed. One list cannot disagree
    /// with itself.
    #[test]
    fn roster_labels_match_the_configured_roster_labels() {
        let cfg = crate::config::Config::default();
        let status: Vec<&str> = crate::channels::channel_status_roster(&cfg)
            .into_iter()
            .map(|(n, _, _, _)| n)
            .collect();
        let configured: Vec<&str> = crate::channels::channel_roster(&cfg)
            .into_iter()
            .map(|(n, _, _, _)| n)
            .collect();
        assert_eq!(status, configured);
    }

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

    fn is_configured(
        rows: &[(
            &'static str,
            bool,
            crate::channels::ChannelSupport,
            crate::channels::ChannelVerification,
        )],
        name: &str,
    ) -> bool {
        rows.iter()
            .find(|(n, _, _, _)| *n == name)
            .map(|(_, c, _, _)| *c)
            .unwrap()
    }

    /// The bug: a `[channels_config.telegram]` block with an empty bot_token —
    /// a hand-edit or aborted /setup — used to render "✓ configured" because
    /// the check was `.is_some()`.
    #[test]
    fn empty_bot_token_reads_as_not_configured() {
        let rows = crate::channels::channel_status_roster(&cfg_with_telegram(""));
        assert!(!is_configured(&rows, "Telegram"));
        let rows = crate::channels::channel_status_roster(&cfg_with_telegram("   "));
        assert!(
            !is_configured(&rows, "Telegram"),
            "whitespace is not a token"
        );
    }

    #[test]
    fn a_real_bot_token_reads_as_configured() {
        let rows = crate::channels::channel_status_roster(&cfg_with_telegram("123:XYZ"));
        assert!(is_configured(&rows, "Telegram"));
    }

    /// DingTalk authenticates with client_id + client_secret (the pair the
    /// channel sends). A block missing either — or with a blank one — is not
    /// configured; app_id/app_secret are unused by the channel and not checked.
    #[test]
    fn dingtalk_requires_client_id_and_secret() {
        let with = |cid: &str, sec: &str| {
            let mut c = crate::config::Config::default();
            c.channels_config.dingtalk = Some(
                serde_json::from_value(serde_json::json!({
                    "client_id": cid,
                    "client_secret": sec,
                    "allowed_users": [],
                    "app_id": "",
                    "app_secret": "",
                }))
                .unwrap(),
            );
            crate::channels::channel_status_roster(&c)
        };
        assert!(is_configured(&with("cid", "sec"), "DingTalk"));
        assert!(!is_configured(&with("", "sec"), "DingTalk"), "no client_id");
        assert!(!is_configured(&with("cid", ""), "DingTalk"), "no secret");
        assert!(!is_configured(&with("  ", "sec"), "DingTalk"), "blank id");
    }

    /// Email needs both hosts (receive + send) and the username/password pair
    /// to authenticate. Missing any one leaves it unconfigured.
    #[test]
    fn email_requires_hosts_and_auth() {
        let cfg = |imap: &str, smtp: &str, user: &str, pass: &str| {
            let mut c = crate::config::Config::default();
            c.channels_config.email = Some(
                serde_json::from_value(serde_json::json!({
                    "imap_host": imap, "imap_port": 993, "imap_folder": "INBOX",
                    "smtp_host": smtp, "smtp_port": 587, "smtp_tls": true,
                    "username": user, "password": pass,
                    "from_address": "a@example.com",
                    "allowed_senders": [],
                }))
                .unwrap(),
            );
            crate::channels::channel_status_roster(&c)
        };
        assert!(is_configured(&cfg("imap.x", "smtp.x", "u", "p"), "Email"));
        assert!(
            !is_configured(&cfg("", "smtp.x", "u", "p"), "Email"),
            "no imap"
        );
        assert!(
            !is_configured(&cfg("imap.x", "", "u", "p"), "Email"),
            "no smtp"
        );
        assert!(
            !is_configured(&cfg("imap.x", "smtp.x", "", "p"), "Email"),
            "no user"
        );
        assert!(
            !is_configured(&cfg("imap.x", "smtp.x", "u", ""), "Email"),
            "no pass"
        );
    }

    #[test]
    fn an_absent_channel_reads_as_not_configured() {
        let rows = crate::channels::channel_status_roster(&crate::config::Config::default());
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
        let rows = crate::channels::channel_status_roster(&c);
        assert!(is_configured(&rows, "iMessage"));
    }

    #[test]
    fn provider_key_status_is_false_for_the_keyless_default() {
        // The exact config the old `!ctx.model.is_empty()` check green-lit:
        // Config::default sets default_provider = openrouter and no key.
        let c = crate::config::Config::default();
        assert_eq!(provider_key_status(&c), Some(false));
    }

    #[test]
    fn provider_key_status_true_with_a_top_level_key() {
        let mut c = crate::config::Config::default();
        c.api_key = Some("sk-test".into());
        assert_eq!(provider_key_status(&c), Some(true));
    }

    #[test]
    fn provider_key_status_true_with_a_per_provider_key() {
        let mut c = crate::config::Config::default();
        c.api_key = None;
        c.provider_api_keys
            .insert("openrouter".into(), "sk-test".into());
        assert_eq!(provider_key_status(&c), Some(true));
    }

    #[test]
    fn provider_key_status_true_for_a_local_provider_without_a_key() {
        let mut c = crate::config::Config::default();
        c.default_provider = Some("ollama".into());
        c.api_key = None;
        assert_eq!(provider_key_status(&c), Some(true));
    }

    #[test]
    fn provider_key_status_none_without_a_default_provider() {
        let mut c = crate::config::Config::default();
        c.default_provider = None;
        assert_eq!(provider_key_status(&c), None);
    }

    #[test]
    fn provider_key_status_rejects_a_blank_key() {
        let mut c = crate::config::Config::default();
        c.api_key = Some("   ".into());
        assert_eq!(provider_key_status(&c), Some(false));
    }

    #[test]
    fn boot_door_opens_the_first_run_wizard_on_a_fresh_install() {
        // A fresh install names a provider but stores no key, and the agent
        // cannot even build (no model configured). The wizard is the door
        // that sets this profile up.
        let c = crate::config::Config::default();
        assert_eq!(
            boot_door(&c, Some("no model configured")),
            BootDoor::FirstRunWizard,
        );
    }

    #[test]
    fn boot_door_opens_the_provider_overlay_when_a_configured_provider_breaks() {
        // A profile that WAS set up: a key resolves for its provider, but
        // that provider fails to build. The provider overlay owns exactly
        // the keys whose current values broke the boot.
        let mut c = crate::config::Config::default();
        c.default_provider = Some("no-such-provider".into());
        c.api_key = Some("sk-test".into());
        assert_eq!(
            boot_door(&c, Some("unknown provider")),
            BootDoor::ProviderOverlay,
        );
    }

    #[test]
    fn boot_door_opens_nothing_on_a_configured_and_healthy_profile() {
        let mut c = crate::config::Config::default();
        c.api_key = Some("sk-test".into());
        assert_eq!(boot_door(&c, None), BootDoor::None);
    }

    #[test]
    fn boot_door_opens_nothing_when_the_credential_lives_in_the_environment() {
        // The key is in the environment, not in config.toml: this config
        // stores no credential, but the agent built, so the install can send
        // and there is nothing to set up. The wizard must not open over it.
        let mut c = crate::config::Config::default();
        c.default_provider = Some("openrouter".into());
        c.default_model = Some("vendor/model".into());
        assert!(!c.provider_credential_resolves());
        assert_eq!(boot_door(&c, None), BootDoor::None);
    }

    #[test]
    fn boot_door_opens_nothing_for_a_keyless_local_provider() {
        // `ollama` needs no credential at all: the agent built and the
        // install can send, so the wizard must not open over it either.
        let mut c = crate::config::Config::default();
        c.default_provider = Some("ollama".into());
        c.default_model = Some("llama3.2".into());
        assert!(!c.provider_credential_resolves());
        assert_eq!(boot_door(&c, None), BootDoor::None);
    }

    #[test]
    fn boot_door_opens_the_first_run_wizard_when_only_the_environment_holds_the_credential() {
        // The provider broke and the only credential is in the environment:
        // this config holds none, so there is nothing here that was "set up"
        // for the overlay to repair — the wizard is the door that sets a
        // credential up.
        let mut c = crate::config::Config::default();
        c.default_provider = Some("openrouter".into());
        c.default_model = Some("vendor/model".into());
        assert!(!c.provider_credential_resolves());
        assert_eq!(
            boot_door(&c, Some("provider failed to start")),
            BootDoor::FirstRunWizard,
        );
    }

    use crate::sessions::SessionStore;
    use crate::tui::context::TuiContext;

    /// Like `make_app_from_store`, but hands back the bridge's request
    /// receiver so a test can observe what the app sends to the agent.
    fn make_app_with_bridge_from_store(
        store: SessionStore,
        model: &str,
    ) -> (TuiApp, tokio::sync::mpsc::Receiver<TurnRequest>) {
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(4);
        let (_events_tx, events_rx) = tokio::sync::mpsc::channel(32);
        let ctx = TuiContext::new(store, model, None, req_tx, events_rx).expect("context");
        let app = TuiApp {
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
            pending_setup_change: SetupChange::None,
            channels_restarted_for_save: false,
            setup_response_tx: None,
            setup_force: false,
            scrollback_queue: Vec::new(),
            selection_drag_anchor: None,
            selection_dragged: false,
            list_picker: None,
            clawhub_install_last_query: String::new(),
            clawhub_install_search_version: 0,
            clawhub_install_results_rx: None,
            clawhub_install_results_tx: None,
            clawhub_install_in_progress: None,
            clawhub_search_frame: None,
            clawhub_install_completion_rx: None,
            clawhub_install_completion_tx: None,
            skill_deps_install_in_progress: None,
            skill_deps_install_completion_rx: None,
            skill_deps_install_completion_tx: None,
            skill_deps_install_finished_at: None,
            skills_watcher: None,
            config_watcher: None,
            config_fingerprint: Arc::new(std::sync::Mutex::new(String::new())),
            model_switch_save_rx: None,
            channel_supervisor: None,
            info_panel: None,
            stream_committed_chars: 0,
            stream_header_committed: false,
            editor_request: false,
            skill_editor_request: None,
            clear_terminal_request: false,
            composer_viewport_rows: INLINE_VIEWPORT_LINES,
            first_run_wizard: None,
            login_gate: None,
            pending_approvals_rx: None,
            shell_blocks_this_turn: 0,
            autonomy_hint_shown_this_turn: false,
            pending_turns: std::collections::VecDeque::new(),
            actor_has_agent: true,
            pending_approval: None,
            pending_reload: None,
            reload_seq: 0,
            model_picker_entries: None,
        };
        (app, req_rx)
    }

    fn make_app_from_store(store: SessionStore, model: &str) -> TuiApp {
        // Tests that hit this helper do not exercise the bridge; the request
        // receiver is dropped with the returned tuple's tail, and the events
        // sender is held inside the helper for the duration of the test.
        make_app_with_bridge_from_store(store, model).0
    }

    // ── `/model` switches the agent, not just the label ──────────
    //
    // `/model` reported "Model set to: X" while `context.model` was the only
    // thing that changed: the agent kept its launch-time provider/model and
    // every subsequent turn silently ran on the old one.

    /// The reload the app sent to the agent bridge, as `(correlation id, config)`.
    async fn expect_reload(
        req_rx: &mut tokio::sync::mpsc::Receiver<TurnRequest>,
    ) -> (u64, Box<crate::config::Config>) {
        let req = tokio::time::timeout(std::time::Duration::from_secs(5), req_rx.recv())
            .await
            .expect("reload sent within 5s")
            .expect("bridge channel open");
        match req {
            TurnRequest::Reload { id, config } => (id, config),
            other => panic!("expected TurnRequest::Reload, got {other:?}"),
        }
    }

    /// Whether a second reload is queued: the watcher must not push one for the
    /// bytes a `/model` switch just wrote.
    async fn no_second_reload(req_rx: &mut tokio::sync::mpsc::Receiver<TurnRequest>) -> bool {
        tokio::time::timeout(std::time::Duration::from_millis(200), req_rx.recv())
            .await
            .is_err()
    }

    #[tokio::test]
    async fn model_selection_persists_config_and_reloads_the_agent() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "anthropic:old-model",
        );
        let path = tmp.path().join("config.toml");
        app.config.config_path = path.clone();
        app.config.workspace_dir = tmp.path().to_path_buf();

        app.apply_model_selection(Some("openrouter"), "meta/llama-4-scout");

        // The status bar does not change until the actor reports the new agent
        // is in place (`ReloadComplete`): until then the agent still runs the
        // old model, and the label must keep saying so.
        assert_eq!(app.context.model, "anthropic:old-model");

        // The reload carries the new pair. Nothing is on disk yet: a switch that
        // could not be built must not be left behind for the next launch.
        let (id, config) = expect_reload(&mut req_rx).await;
        assert_eq!(config.default_provider.as_deref(), Some("openrouter"));
        assert_eq!(config.default_model.as_deref(), Some("meta/llama-4-scout"));
        assert!(!path.exists(), "the file waits for the rebuild");

        // The agent is in place: the label names the switch and the config that
        // was built from it is now what `self.config` and the file hold.
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });
        assert_eq!(app.context.model, "openrouter:meta/llama-4-scout");
        assert_eq!(
            app.config.default_model.as_deref(),
            Some("meta/llama-4-scout")
        );

        let mut saved = String::new();
        for _ in 0..100 {
            if path.exists() {
                saved = std::fs::read_to_string(&path).expect("config persisted to disk");
                if saved.contains("meta/llama-4-scout") {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            saved.contains("meta/llama-4-scout"),
            "the config an agent was built from is the one on disk"
        );
    }

    /// `/model nosuch:xyz` used to save `default_provider = "nosuch"` and
    /// reload into a build that fails, so the next launch opened setup. A
    /// target that cannot be built is refused before anything is saved or
    /// reloaded, and the status bar and the config keep what they had.
    #[tokio::test]
    async fn model_selection_refuses_a_provider_that_cannot_run() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        app.config.config_path = tmp.path().join("config.toml");
        app.config.workspace_dir = tmp.path().to_path_buf();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("old-model".to_string());

        app.apply_model_selection(Some("nosuch"), "xyz");

        assert_eq!(
            app.context.model, "openai:old-model",
            "the status bar keeps the model the agent is running"
        );
        assert_eq!(
            app.config.default_provider.as_deref(),
            Some("openai"),
            "the in-memory config keeps the provider it had"
        );
        assert_eq!(app.config.default_model.as_deref(), Some("old-model"));
        assert!(
            app.context
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("nosuch")),
            "the refusal names the provider: {:?}",
            app.context.last_error
        );
        assert!(
            !tmp.path().join("config.toml").exists(),
            "nothing is saved for a refused target"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), req_rx.recv())
                .await
                .is_err(),
            "no Reload is sent for a refused target"
        );
    }

    /// The status bar follows the agent: it moves only when the reload that a
    /// `/model` switch triggered reports the new agent in place, and then it
    /// reads `provider:model` — the boot label — even for a bare `/model name`.
    #[tokio::test]
    async fn status_bar_adopts_the_model_only_when_the_reload_completes() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        app.config.config_path = tmp.path().join("config.toml");
        app.config.workspace_dir = tmp.path().to_path_buf();
        app.config.default_provider = Some("openai".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());

        app.apply_model_selection(None, "new-model");
        let (id, _config) = expect_reload(&mut req_rx).await;
        assert_eq!(app.context.model, "openai:old-model", "not yet in place");

        app.handle_agent_event(AgentEvent::ReloadComplete {
            id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });
        assert_eq!(
            app.context.model, "openai:new-model",
            "a bare /model name is labelled like the boot label"
        );
        assert!(
            app.pending_reload.is_none(),
            "the pending switch is consumed"
        );
    }

    /// An unrelated reload sent AFTER a `/model` switch and completed after it
    /// carries the switch's model. The actor is FIFO and builds its agent from
    /// the config a `Reload` hands it, so the reload that lands last is the one
    /// the next message is answered on: carrying `self.config` as it stands
    /// would put the agent back on the pre-switch model while the status bar
    /// and config.toml both named the new one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unrelated_reload_sent_during_a_switch_carries_the_switches_model() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        let path = tmp.path().join("config.toml");
        app.config.config_path = path;
        app.config.workspace_dir = tmp.path().to_path_buf();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("old-model".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());

        app.apply_model_selection(Some("openai"), "new-model");
        let (switch_id, _switch_config) = expect_reload(&mut req_rx).await;
        assert_eq!(
            app.context.model, "openai:old-model",
            "the switch has not been rebuilt yet"
        );

        // `/autonomy` saves and hands its own config to the actor — a reload
        // the switch did not ask for, sent after it.
        app.apply_preset_to_config_and_reload(crate::approval::policy_writer::PolicyPreset::Manual)
            .expect("preset applied and saved");
        let (other_id, other_config) = expect_reload(&mut req_rx).await;
        assert_eq!(
            other_id,
            switch_id + 1,
            "sent after the switch, so it is the reload that lands last"
        );
        assert_eq!(
            other_config.default_model.as_deref(),
            Some("new-model"),
            "the last reload to land decides what the agent answers with, so it carries the \
             model the switch is waiting to put in place"
        );

        // Both rebuilds land, the switch's first — the actor is FIFO, so the
        // agent at rest is built from the unrelated reload's config.
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: switch_id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: other_id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });

        assert_eq!(app.context.model, "openai:new-model");
        assert_eq!(
            (
                app.config.default_provider.as_deref(),
                app.config.default_model.as_deref(),
            ),
            (Some("openai"), Some("new-model")),
            "the config the agent answers with at rest names the model the label does"
        );
        assert_eq!(
            app.config.autonomy.always_ask,
            vec!["*".to_string()],
            "the unrelated reload's own change survives the switch adopting its model"
        );
    }

    /// A config-watcher reload that lands while a `/model` switch is in flight
    /// — what an operator's hand edit of config.toml becomes — keeps the switch
    /// instead of superseding it. The edit reaches the agent, the switch's model
    /// rides on the edit's own rebuild, and the switch's own outcome still
    /// adopts it, so label, agent and file agree once everything settles.
    #[tokio::test]
    async fn a_hand_edit_during_a_switch_keeps_the_switch_and_its_model() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path().to_path_buf();
        let path = root.join("config.toml");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        // `reload_config` reads the profile's config.toml, so the hand edit, the
        // switch's save and the reload must all be looking at this one file.
        app.config.config_path = path.clone();
        app.config.workspace_dir = root.clone();
        app.profile.root = root.clone();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("old-model".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());
        app.config.save().await.expect("baseline config written");

        app.apply_model_selection(Some("openai"), "new-model");
        let (switch_id, _switch_config) = expect_reload(&mut req_rx).await;

        // The operator edits the file while that rebuild is still in flight.
        // The edit touches something other than the model, so it competes with
        // the switch for nothing except the reload slot.
        let mut edited = app.config.clone();
        edited.default_temperature = 0.25;
        edited.save().await.expect("hand edit written");
        app.reload_config().expect("the edit reloads");
        let (edit_id, edit_config) = expect_reload(&mut req_rx).await;

        assert_eq!(edit_id, switch_id + 1, "sent after the switch");
        assert_eq!(
            edit_config.default_temperature, 0.25,
            "the edit reaches the agent"
        );
        assert_eq!(
            edit_config.default_model.as_deref(),
            Some("new-model"),
            "the file still names the pre-switch model, so this rebuild must carry the \
             switch's — otherwise the agent goes back to a model the label has not named"
        );
        assert_eq!(
            app.pending_reload.as_ref().map(|p| p.id),
            Some(switch_id),
            "the switch keeps the pending slot instead of being dropped by the watcher tick"
        );
        assert!(
            app.pending_reload
                .as_ref()
                .is_some_and(|p| p.unsaved_config.is_some()),
            "the dropped part of a switch is the config it has not written yet"
        );
        assert_eq!(
            app.context.model, "openai:old-model",
            "the label has not moved: no agent has reported the new one yet"
        );

        // Both rebuilds land, the switch's first.
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: switch_id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: edit_id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });

        assert_eq!(app.context.model, "openai:new-model");
        assert_eq!(
            (
                app.config.default_model.as_deref(),
                app.config.default_temperature,
            ),
            (Some("new-model"), 0.25),
            "the config the agent answers with at rest carries both the switch's model and the edit"
        );

        // The file lands on the same pair: the switch's save is the last writer.
        let mut on_disk = String::new();
        for _ in 0..150 {
            on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            if on_disk.contains("new-model") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            on_disk.contains("new-model"),
            "the file names the new model"
        );
        assert!(on_disk.contains("0.25"), "the edit survived the save");
    }

    /// A `/model` switch whose rebuild fails changes nothing. The status bar
    /// keeps naming the model the agent still runs, `self.config` and the file
    /// on disk keep the pair they had, and the operator reads one line.
    #[tokio::test]
    async fn failed_model_switch_leaves_the_label_the_config_and_the_file_alone() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        let path = tmp.path().join("config.toml");
        app.config.config_path = path.clone();
        app.config.workspace_dir = tmp.path().to_path_buf();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("old-model".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());
        app.config.save().await.expect("baseline config written");
        let before = std::fs::read(&path).expect("baseline bytes");

        app.apply_model_selection(Some("openai"), "new-model");
        let (id, config) = expect_reload(&mut req_rx).await;
        assert_eq!(config.default_model.as_deref(), Some("new-model"));
        app.handle_agent_event(AgentEvent::ReloadFailed {
            id,
            error: "the new provider refused to build".to_string(),
        });

        assert_eq!(
            app.context.model, "openai:old-model",
            "the label keeps naming the model the agent runs"
        );
        assert_eq!(
            app.config.default_model.as_deref(),
            Some("old-model"),
            "the in-memory config keeps the pair it had"
        );
        assert_eq!(
            std::fs::read(&path).expect("config still there"),
            before,
            "a switch that could not be built is never written to disk"
        );
        let said: Vec<&str> = app
            .scrollback_queue
            .iter()
            .map(|(_, m)| m.as_str())
            .collect();
        assert_eq!(said.len(), 1, "the refusal is said once: {said:?}");
        assert!(
            said[0].contains("Model not changed") && said[0].contains("refused to build"),
            "the line says the model was not changed and why: {said:?}"
        );
    }

    /// The switch's save runs off-thread, so "Model set to" is said before the
    /// write is known to have landed. A write that fails is said too: the agent
    /// really is on the new model, but config.toml is stale, and an operator told
    /// only the first thing will read a file that disagrees with the status bar.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_model_whose_save_fails_says_so_and_keeps_the_session_switch() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        // A directory where the file goes: `atomic_write_config` writes its temp
        // file and then cannot replace a directory with it, so the save fails the
        // way an unwritable path would.
        let path = tmp.path().join("config.toml");
        std::fs::create_dir(&path).expect("the config path is a directory");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        app.config.config_path = path;
        app.config.workspace_dir = tmp.path().to_path_buf();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("old-model".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());

        app.apply_model_selection(Some("openai"), "new-model");
        let (id, _config) = expect_reload(&mut req_rx).await;
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });
        assert_eq!(app.context.model, "openai:new-model");

        // The save task reports through the app's drain, as the render loop runs it.
        let mut failures: Vec<String> = Vec::new();
        for _ in 0..150 {
            app.drain_events();
            failures = app
                .scrollback_queue
                .iter()
                .map(|(_, m)| m.clone())
                .filter(|m| m.contains("config.toml"))
                .collect();
            if !failures.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            failures.len(),
            1,
            "one line says the save did not land: {failures:?}"
        );
        assert!(
            failures[0].contains("openai:new-model") && failures[0].contains("this session"),
            "the line says the switch stands for this session: {:?}",
            failures[0]
        );
        assert!(
            app.context
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("config.toml")),
            "the failure is on the error surface too: {:?}",
            app.context.last_error
        );
        assert_eq!(
            app.context.model, "openai:new-model",
            "an agent really is running that model, so the label keeps it"
        );
    }

    /// The switch's own save trips the config watcher. With a REAL watcher
    /// running, that tick must not push a second rebuild of the config the
    /// switch just rebuilt from: one rebuild, one line, and a label that does
    /// not move. A later edit that really changes the file must still reload —
    /// otherwise "no second reload" would only mean the watcher was dead.
    #[tokio::test]
    async fn the_models_own_save_does_not_start_a_second_rebuild() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path().to_path_buf();
        let path = root.join("config.toml");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        app.config.config_path = path.clone();
        app.config.workspace_dir = root.clone();
        // `reload_config` reads the profile's config.toml, so the watcher, the
        // save and the reload must all be looking at this one file.
        app.profile.root = root.clone();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("old-model".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());
        app.config.save().await.expect("baseline config written");
        app.config_watcher = Some(
            crate::config::watcher::ConfigWatcher::watch(&path).expect("watcher on the tempdir"),
        );

        app.apply_model_selection(Some("openai"), "new-model");
        let (id, _config) = expect_reload(&mut req_rx).await;
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });
        assert_eq!(app.context.model, "openai:new-model");

        // Let the save land and its debounced tick arrive (the watcher debounces
        // for 500 ms of real time), draining the tick as the frame loop does.
        let mut on_disk = String::new();
        for _ in 0..150 {
            on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            app.drain_config_reload_events();
            if on_disk.contains("new-model") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            on_disk.contains("new-model"),
            "the switch is saved once the agent runs it"
        );
        for _ in 0..150 {
            app.drain_config_reload_events();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        assert!(
            no_second_reload(&mut req_rx).await,
            "the switch's own save must not push a second rebuild"
        );
        assert_eq!(
            app.context.model, "openai:new-model",
            "the watcher tick does not move the label"
        );
        let said: Vec<&str> = app
            .scrollback_queue
            .iter()
            .map(|(_, m)| m.as_str())
            .collect();
        assert_eq!(said.len(), 1, "the switch is announced once: {said:?}");

        // The watcher is live: an edit that really changes the file reloads.
        let mut edited = app.config.clone();
        edited.default_model = Some("edited-model".to_string());
        edited.save().await.expect("external edit written");
        let mut reloaded: Option<Box<crate::config::Config>> = None;
        for _ in 0..150 {
            app.drain_config_reload_events();
            match tokio::time::timeout(std::time::Duration::from_millis(5), req_rx.recv()).await {
                Ok(Some(TurnRequest::Reload { id, config })) => {
                    assert_ne!(id, 1, "this is the edit's own reload");
                    reloaded = Some(config);
                    break;
                }
                // No rebuild yet, or nothing on the channel this tick.
                Ok(Some(_)) | Err(_) => {}
                Ok(None) => panic!("bridge channel closed"),
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let reloaded = reloaded.expect("a real edit still reloads the agent");
        assert_eq!(reloaded.default_model.as_deref(), Some("edited-model"));
        assert_eq!(
            app.context.model, "openai:new-model",
            "an edit that triggered a rebuild does not name the model before the agent runs it"
        );
    }

    /// On a `custom:<url>` session, a picker row (`custom:<url>:<model>`) is a
    /// real switch: the saved config keeps the whole endpoint as the provider
    /// and the row's model, and the build check accepts it.
    #[tokio::test]
    async fn model_selection_on_a_custom_session_saves_the_endpoint_and_the_model() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let url = "custom:http://127.0.0.1:28521/v1";
        let (mut app, mut req_rx) =
            make_app_with_bridge_from_store(SessionStore::in_memory().expect("store"), url);
        app.config.config_path = tmp.path().join("config.toml");
        app.config.workspace_dir = tmp.path().to_path_buf();
        app.config.default_provider = Some(url.to_string());
        app.config.default_model = Some("llama-a".to_string());

        app.apply_model_selection(Some(url), "llama-b");

        assert!(
            app.context.last_error.is_none(),
            "the build check accepts the endpoint: {:?}",
            app.context.last_error
        );
        let (_id, config) = expect_reload(&mut req_rx).await;
        assert_eq!(config.default_provider.as_deref(), Some(url));
        assert_eq!(config.default_model.as_deref(), Some("llama-b"));
    }

    #[tokio::test]
    async fn bare_model_switch_keeps_the_current_provider() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "anthropic:old-model",
        );
        app.config.config_path = tmp.path().join("config.toml");
        app.config.workspace_dir = tmp.path().to_path_buf();
        app.config.default_provider = Some("anthropic".to_string());
        // A build needs a credential: an operator switching to anthropic has one.
        app.config.api_key = Some("sk-ant-test-placeholder".to_string());

        app.apply_model_selection(None, "claude-opus-5");

        let (_id, config) = expect_reload(&mut req_rx).await;
        assert_eq!(config.default_provider.as_deref(), Some("anthropic"));
        assert_eq!(config.default_model.as_deref(), Some("claude-opus-5"));
    }

    /// A picker row reaches the switch as two values, so an endpoint whose name
    /// holds a colon and a model id that holds a slash both survive. Joined
    /// into one string they came back mangled: the `Qwen/` id split inside
    /// itself, and the portless endpoint split at its own last colon.
    #[tokio::test]
    async fn a_picker_row_keeps_the_whole_endpoint_and_a_model_id_with_a_slash() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("state")).expect("workspace state dir");
        let endpoint = "custom:http://h:8000/v1";
        let portless = "custom:http://localhost:11434";
        std::fs::write(
            workspace.join("state").join("models_cache.json"),
            serde_json::json!({
                "entries": [
                    { "provider": endpoint, "fetched_at_unix": 4_102_444_800_u64,
                      "models": ["Qwen/Qwen2.5-7B"] },
                    { "provider": portless, "fetched_at_unix": 4_102_444_800_u64,
                      "models": ["llama3"] },
                ]
            })
            .to_string(),
        )
        .expect("models cache");

        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        app.config.config_path = tmp.path().join("config.toml");
        app.config.workspace_dir = tmp.path().to_path_buf();
        app.config.default_provider = Some("openai".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());
        app.context.available_providers = vec![endpoint.to_string(), portless.to_string()];
        app.context.workspace_dir = Some(workspace);

        // Row 0: the endpoint with a path, whose model id holds a `/`.
        app.handle_command("model")
            .await
            .expect("/model opens the picker");
        app.list_picker.as_mut().expect("picker open").selected = 0;
        app.dispatch_list_picker_selection().await;
        let (_id, config) = expect_reload(&mut req_rx).await;
        assert_eq!(
            config.default_provider.as_deref(),
            Some(endpoint),
            "the whole endpoint is the provider"
        );
        assert_eq!(
            config.default_model.as_deref(),
            Some("Qwen/Qwen2.5-7B"),
            "a model id that holds a slash is not split inside itself"
        );

        // Row 1: an endpoint with no path at all.
        app.handle_command("model")
            .await
            .expect("/model opens the picker");
        app.list_picker.as_mut().expect("picker open").selected = 1;
        app.dispatch_list_picker_selection().await;
        let (_id, config) = expect_reload(&mut req_rx).await;
        assert_eq!(config.default_provider.as_deref(), Some(portless));
        assert_eq!(config.default_model.as_deref(), Some("llama3"));
    }

    /// A `/model` switch pending with id N, when the actor instead reports an
    /// outcome for the next reload it ran (id N+1 — what an `/autonomy`
    /// triggered after the switch would overwrite the actor slot to), still
    /// adopts the switch: the latest reload sent after a pending switch carries
    /// the switch's model, so its verdict is the switch's. The status bar,
    /// `self.config`, the file and the message all move together, and the
    /// pending slot clears.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_switch_landing_through_a_later_reloads_outcome_adopts_the_switch() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path().to_path_buf();
        let path = root.join("config.toml");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        app.config.config_path = path.clone();
        app.config.workspace_dir = root.clone();
        app.profile.root = root.clone();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("old-model".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());
        app.config.save().await.expect("baseline config written");

        app.apply_model_selection(Some("openai"), "new-model");
        let (switch_id, _switch_config) = expect_reload(&mut req_rx).await;

        // Simulate the actor overwriting the switch's Reload in its single
        // deferred slot: it now holds a Reload stamped N+1 (the later reload
        // the operator triggered). The TUI's `pending_reload` is still the
        // switch's slot, with id N.
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: switch_id + 1,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });

        assert_eq!(
            app.context.model, "openai:new-model",
            "the latest reload carried the switch's model, so its verdict is the switch's"
        );
        assert_eq!(
            app.config.default_provider.as_deref(),
            Some("openai"),
            "self.config follows the switch"
        );
        assert_eq!(
            app.config.default_model.as_deref(),
            Some("new-model"),
            "self.config follows the switch"
        );

        // The "Model set to" line is said exactly once.
        let said: Vec<&str> = app
            .scrollback_queue
            .iter()
            .map(|(_, m)| m.as_str())
            .filter(|m| m.contains("Model set to"))
            .collect();
        assert_eq!(said.len(), 1, "the switch is announced once: {said:?}");
        assert!(
            said[0].contains("openai:new-model"),
            "the line names the new model: {said:?}"
        );

        // The pending slot cleared so the session is not wedged by a stuck switch.
        assert!(
            app.pending_reload.is_none(),
            "the verdict clears the pending slot — nothing left behind for next /model"
        );
        assert!(
            !app.model_switch_in_flight(),
            "no switch is in flight, so a later reload_config takes the slot"
        );

        // The save lands on disk in the background; wait for it.
        let mut on_disk = String::new();
        for _ in 0..150 {
            on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            if on_disk.contains("new-model") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            on_disk.contains("new-model"),
            "the file names the new model"
        );
    }

    /// A `/model` switch pending with id N, when the actor reports a failure
    /// from the same later reload (id N+1), drops the switch: it never landed
    /// on `self.config` or the file, the label stays as it was, and the
    /// operator reads exactly one "Model not changed" line.
    #[tokio::test]
    async fn a_switch_failed_through_a_later_reloads_outcome_drops_the_switch() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path().to_path_buf();
        let path = root.join("config.toml");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        app.config.config_path = path.clone();
        app.config.workspace_dir = root.clone();
        app.profile.root = root.clone();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("old-model".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());
        app.config.save().await.expect("baseline config written");
        let before = std::fs::read(&path).expect("baseline bytes");

        app.apply_model_selection(Some("openai"), "new-model");
        let (switch_id, _switch_config) = expect_reload(&mut req_rx).await;

        // Actor reports failure for the later reload it actually ran (id N+1).
        app.handle_agent_event(AgentEvent::ReloadFailed {
            id: switch_id + 1,
            error: "the new provider refused to build".to_string(),
        });

        assert_eq!(
            app.context.model, "openai:old-model",
            "the label keeps the model the agent still runs"
        );
        assert_eq!(
            app.config.default_provider.as_deref(),
            Some("openai"),
            "self.config keeps what it had"
        );
        assert_eq!(
            app.config.default_model.as_deref(),
            Some("old-model"),
            "self.config keeps what it had"
        );
        assert_eq!(
            std::fs::read(&path).expect("config still there"),
            before,
            "no save happens for a failed switch"
        );
        let said: Vec<&str> = app
            .scrollback_queue
            .iter()
            .map(|(_, m)| m.as_str())
            .collect();
        assert_eq!(said.len(), 1, "the refusal is said once: {said:?}");
        assert!(
            said[0].contains("Model not changed"),
            "the line names what was not changed: {said:?}"
        );
        assert!(
            app.pending_reload.is_none(),
            "the verdict clears the pending slot"
        );
        assert!(
            !app.model_switch_in_flight(),
            "no switch is in flight anymore"
        );
    }

    /// An outcome whose id is smaller than the pending id moves nothing: the
    /// verdict is for a reload the actor ran BEFORE the switch, so the switch
    /// stays pending and the label, `self.config` and the file are untouched.
    #[tokio::test]
    async fn an_outcome_with_a_smaller_id_moves_nothing() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path().to_path_buf();
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        app.config.config_path = root.join("config.toml");
        app.config.workspace_dir = root.clone();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("old-model".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());

        app.apply_model_selection(Some("openai"), "new-model");
        let (switch_id, _switch_config) = expect_reload(&mut req_rx).await;

        // An outcome for a reload that ran BEFORE the switch.
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: switch_id - 1,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });

        assert_eq!(
            app.context.model, "openai:old-model",
            "an older verdict does not move the label"
        );
        assert_eq!(
            app.config.default_model.as_deref(),
            Some("old-model"),
            "an older verdict does not adopt its model"
        );
        assert!(
            app.pending_reload.is_some(),
            "the switch keeps waiting on its own verdict"
        );
        assert!(app.model_switch_in_flight(), "a switch is still in flight");

        // The switch's own outcome (with the exact pending id) still lands.
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: switch_id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });
        assert_eq!(app.context.model, "openai:new-model");
        assert!(app.pending_reload.is_none());
    }

    /// After a switch's verdict (even the one that landed through a later
    /// reload's outcome), a fresh `reload_config` from a file naming another
    /// provider takes its own slot and its own outcome moves the label —
    /// the previously stuck switch no longer wedges the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn after_a_verdict_a_later_config_reload_takes_its_own_slot_and_moves_the_label() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let root = tmp.path().to_path_buf();
        let path = root.join("config.toml");
        let (mut app, mut req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:old-model",
        );
        app.config.config_path = path.clone();
        app.config.workspace_dir = root.clone();
        app.profile.root = root.clone();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("old-model".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());
        app.config.save().await.expect("baseline config written");

        app.apply_model_selection(Some("openai"), "new-model");
        let (switch_id, _switch_config) = expect_reload(&mut req_rx).await;

        // The later reload the operator triggered lands the switch's model
        // (`with_pending_model`), so its outcome is the switch's verdict. Send
        // it for real — not a fabricated id — so the app's reload counter
        // advances exactly as the live path does.
        app.apply_preset_to_config_and_reload(crate::approval::policy_writer::PolicyPreset::Manual)
            .expect("preset applied and saved");
        let (later_id, later_config) = expect_reload(&mut req_rx).await;
        assert_eq!(later_id, switch_id + 1);
        assert_eq!(
            later_config.default_model.as_deref(),
            Some("new-model"),
            "the later reload carries the switch's model"
        );
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: later_id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });
        assert_eq!(app.context.model, "openai:new-model");
        assert!(app.pending_reload.is_none());

        // Wait for the switch's save to land on disk before the operator
        // edits the file again.
        let mut saved_baseline = String::new();
        for _ in 0..150 {
            saved_baseline = std::fs::read_to_string(&path).unwrap_or_default();
            if saved_baseline.contains("new-model") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            saved_baseline.contains("new-model"),
            "the switch's save has landed"
        );

        // Operator hands edits the file to a different provider/model.
        let mut edited = app.config.clone();
        edited.default_provider = Some("other-provider".to_string());
        edited.default_model = Some("other-model".to_string());
        edited.save().await.expect("hand edit written");

        // TUI reloads from the file. Anything from here on belongs to this
        // plain reload; the switch's own "Model set to" line is already in the
        // scrollback and must not be re-counted.
        let before_reload = app.scrollback_queue.len();
        app.reload_config().expect("the hand edit reloads");
        let (reload_id, reload_config) = expect_reload(&mut req_rx).await;

        // The reload took its own slot — its own id, no unsaved_config.
        let pending = app
            .pending_reload
            .as_ref()
            .expect("the reload takes its own slot now");
        assert_eq!(
            pending.id, reload_id,
            "the pending slot matches the reload the app sent"
        );
        assert_eq!(
            reload_id,
            switch_id + 2,
            "this reload is its own event, not the old switch's"
        );
        assert_eq!(
            reload_config.default_provider.as_deref(),
            Some("other-provider"),
            "the hand edit reaches the agent"
        );
        assert_eq!(
            reload_config.default_model.as_deref(),
            Some("other-model"),
            "the hand edit reaches the agent"
        );
        assert!(
            pending.unsaved_config.is_none(),
            "a plain config reload has nothing unsaved to persist"
        );

        // The reload's own outcome moves the label.
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: reload_id,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });
        assert_eq!(
            app.context.model, "other-provider:other-model",
            "the new file's model lands on the label"
        );
        assert!(
            app.pending_reload.is_none(),
            "the reload's own verdict clears the slot"
        );

        // A plain config reload whose file matches what the agent already
        // runs must not claim the model was set — there is nothing unsaved
        // to announce. Scoped to this reload's own messages: the switch's
        // verdict above legitimately said "Model set to".
        let said: Vec<&str> = app
            .scrollback_queue
            .iter()
            .skip(before_reload)
            .map(|(_, m)| m.as_str())
            .collect();
        assert!(
            !said.iter().any(|m| m.contains("Model set to")),
            "a plain config reload does not say \"Model set to\": {said:?}"
        );
    }

    /// A plain config reload (`unsaved_config: None`) pending with id N, when
    /// the actor reports `ReloadComplete { id: N+1 }` for the later reload it
    /// actually ran, still moves the label — the latest reload carries the
    /// same model either way, and no "Model set to" line is announced because
    /// there is nothing unsaved to persist.
    #[tokio::test]
    async fn a_plain_config_reload_moves_the_label_through_a_later_outcome() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let (mut app, _req_rx) = make_app_with_bridge_from_store(
            SessionStore::in_memory().expect("store"),
            "openai:edited-model",
        );
        app.config.config_path = tmp.path().join("config.toml");
        app.config.workspace_dir = tmp.path().to_path_buf();
        app.config.default_provider = Some("openai".to_string());
        app.config.default_model = Some("edited-model".to_string());
        app.config.api_key = Some("sk-test-placeholder".to_string());

        // Place a plain config reload in the pending slot directly, bypassing
        // `reload_config` (which would also kick the actor). The semantics of
        // a plain reload are what `reload_config` builds when no switch is in
        // flight: `model_label` from the file, `unsaved_config: None`.
        let id = app.next_reload_id();
        let model_label = model_label_of(&app.config);
        app.pending_reload = Some(PendingReload {
            id,
            model_label: Some(model_label),
            unsaved_config: None,
        });

        // Later outcome that arrived after the actor overwrote.
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: id + 1,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });

        assert_eq!(
            app.context.model, "openai:edited-model",
            "a plain config reload moves the label through a later outcome"
        );
        assert!(
            app.pending_reload.is_none(),
            "the verdict clears the pending slot"
        );
        let said: Vec<&str> = app
            .scrollback_queue
            .iter()
            .map(|(_, m)| m.as_str())
            .collect();
        assert!(
            said.is_empty(),
            "no \"Model set to\" line for a reload that persists nothing: {said:?}"
        );
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
        assert_eq!(app.context.messages[0].role, "note");
        assert_ne!(app.context.session_id, first_session_id);
    }

    /// The shell-block hint names the preset the profile has, and says plainly
    /// that no preset is set when the profile has none. The hint used to fall
    /// back to "Smart", because the config cannot tell "no preset chosen" from
    /// "the default" — so a profile that chose nothing was told a policy was
    /// in force. Naming any preset there was the same bug in a different
    /// coat, and staying silent about it left the reader unable to tell
    /// "nothing was chosen" from "a policy is in force but unnamed".
    #[test]
    fn the_shell_block_hint_names_no_preset_without_one() {
        use crate::approval::policy_writer::PolicyPreset;

        let hint = shell_block_hint(None);
        assert!(
            !hint.contains("Smart"),
            "a profile with no preset must not be told it is under Smart:\n{hint}"
        );
        // The other three labels: a `None` arm naming Manual, Strict or Off
        // instead of Smart would be the same bug wearing a different label.
        // The advice that follows ("/autonomy off", "/autonomy smart") names
        // the commands in lower case, so a preset label cannot hide there.
        for preset in [
            PolicyPreset::Manual,
            PolicyPreset::Strict,
            PolicyPreset::Off,
        ] {
            assert!(
                !hint.contains(preset.label()),
                "a profile with no preset must not be told it is under {}:\n{hint}",
                preset.label()
            );
        }
        assert!(
            hint.contains("approval policy"),
            "the hint must still say what blocked the commands:\n{hint}"
        );
        assert!(
            hint.contains("no preset is set"),
            "the hint must say that no preset is set — silence reads as a policy \
             in force:\n{hint}"
        );

        let hint = shell_block_hint(Some(PolicyPreset::Manual));
        assert!(
            hint.contains("Manual preset"),
            "a profile with a preset names it:\n{hint}"
        );
    }

    /// The cron detail panel's action keys are write doors: run, pause/resume
    /// and delete. With the cron feature switch off they must refuse with the
    /// same words every other door uses — and change nothing.
    #[tokio::test]
    async fn the_cron_panel_action_keys_refuse_when_cron_is_disabled() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let mut config = crate::config::Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..crate::config::Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let job = crate::cron::add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        config.cron.enabled = false;

        for action in ['r', 'p', 'd'] {
            let store = SessionStore::in_memory().expect("store");
            let mut app = make_app_from_store(store, "test-model");
            app.info_panel = TuiApp::build_cron_detail_panel(&config, &job.id);
            assert!(
                app.info_panel.is_some(),
                "the detail panel must be open for the {action} key"
            );

            app.cron_panel_action_with_config(action, &config);

            let said: Vec<String> = app
                .scrollback_queue
                .iter()
                .map(|(_, m)| m.clone())
                .collect();
            assert!(
                said.iter()
                    .any(|m| m.contains(crate::cron::CRON_DISABLED_MESSAGE)),
                "the {action} key must refuse: {said:?}"
            );

            // Nothing ran, nothing was paused, nothing was deleted.
            let stored = crate::cron::get_job(&config, &job.id).unwrap();
            assert!(stored.enabled, "the {action} key must not pause the job");
            assert!(
                crate::cron::list_runs(&config, &job.id, 10)
                    .unwrap()
                    .is_empty(),
                "the {action} key must not run the job"
            );
            assert_eq!(
                crate::cron::list_jobs(&config).unwrap().len(),
                1,
                "the {action} key must not delete the job"
            );
        }
    }
}

#[cfg(test)]
mod submit_tests {
    use super::*;
    use crate::tui::async_bridge::TurnRequest;
    use crate::tui::context::TuiContext;

    use super::test_support::app_with_context as make_app_with_context;
    /// When several shell calls block at once (parallel tool execution),
    /// approving the one shown in the box must advance the box to the next
    /// still-queued request — not leave it blank, stranding the other blocked
    /// call off-screen where the agent looks frozen.
    #[tokio::test]
    async fn approving_one_of_several_pending_advances_the_box_to_the_next() {
        use std::sync::Arc;
        use std::time::Duration;

        let (mut ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let security = Arc::new(crate::security::SecurityPolicy::default());
        let registry = Arc::new(crate::security::PendingApprovals::new(Some(
            Duration::from_secs(5),
        )));
        security.set_pending(registry.clone());
        ctx.security = Some(security);

        // Two shell calls blocked concurrently on distinct basenames.
        let r1 = registry.clone();
        let t1 = tokio::spawn(async move { r1.request_decision("aa", "aa --x", "tui").await });
        let r2 = registry.clone();
        let t2 = tokio::spawn(async move { r2.request_decision("bb", "bb --y", "tui").await });
        for _ in 0..200 {
            if registry.list().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(registry.list().len(), 2, "both requests registered");

        let mut app = make_app_with_context(ctx);
        // The box currently shows one of them; approve it.
        let shown_first = registry.list().into_iter().next().unwrap();
        let resolved_name = shown_first.basename.clone();
        app.pending_approval = Some(shown_first);
        app.resolve_pending_approval(crate::security::Decision::Session);

        // The box must now show the OTHER, still-live request — not go blank
        // and not re-show the one just resolved.
        let advanced = app
            .pending_approval
            .as_ref()
            .map(|r| r.basename.clone())
            .expect("box must advance to the remaining queued approval, not go blank");
        assert_ne!(
            advanced, resolved_name,
            "the box must not re-show the just-resolved request"
        );

        // Clean up the spawned waiters so the test doesn't leak tasks.
        registry.resolve_by_basename(&advanced, crate::security::Decision::Session);
        let _ = t1.await;
        let _ = t2.await;
    }

    /// The status pane's text, so a test can read what the operator reads.
    fn status_pane_text(ctx: &TuiContext, w: u16, h: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_status_pane(ctx, &AppState::Ready, f, area);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect()
    }

    /// The status line rendered `0` for a session no provider reported usage
    /// for, next to the model name, as though it were a measurement. A turn
    /// that consumed zero tokens does not exist.
    #[test]
    fn the_status_line_shows_a_dash_when_no_usage_was_reported() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        assert!(ctx.token_usage.is_none(), "fixture starts unreported");

        // Assert the token slot itself — the line also carries "0 msgs" and
        // "0s", which are real zeros and must stay.
        let text = status_pane_text(&ctx, 100, 1);
        assert!(
            text.contains("mock-model  │  —  │"),
            "an unreported count must read as unknown, not as 0: {text:?}"
        );
    }

    /// `/setup <locked-channel>` must refuse before it ever spawns the
    /// provisioner task — no config write, no overlay.
    #[test]
    fn open_setup_overlay_refuses_a_locked_channel() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        let err = app
            .open_setup_overlay("irc".to_string())
            .expect_err("irc is under development and must be refused");
        // Checks the sentence's own wording, not the shared "under
        // development" label string — that string is defined exactly once
        // in `ChannelSupport::label()`, and a second quoted copy here would
        // be exactly the drift `check_channel_maturity.sh` exists to catch.
        assert!(
            err.to_string().contains("cannot be set up yet"),
            "got: {err}"
        );
        assert!(
            app.setup_overlay.is_none(),
            "a refused channel must never open the setup overlay"
        );
    }

    /// A usable channel is unaffected by the lock. `open_setup_overlay`
    /// spawns the provisioner's task on success, so this needs a runtime —
    /// the refusal case above does not, since it returns before ever
    /// reaching the spawn.
    #[tokio::test]
    async fn open_setup_overlay_still_opens_a_usable_channel() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.open_setup_overlay("telegram".to_string())
            .expect("telegram is usable and must open");
        assert!(app.setup_overlay.is_some());
    }

    /// The other half: a reported count still renders as a number.
    #[test]
    fn the_status_line_shows_the_count_a_provider_reported() {
        let (mut ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx.token_usage = Some(crate::tui::context::TokenUsage {
            prompt_tokens: 900,
            completion_tokens: 334,
            total_tokens: 1234,
        });

        let text = status_pane_text(&ctx, 100, 1);
        assert!(
            text.contains("mock-model  │  1.2K  │"),
            "a reported count must be shown as a number: {text:?}"
        );
    }

    fn chat_pane_buffer_text(ctx: &mut TuiContext, w: u16, h: u16) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_chat_pane(&AppState::Ready, ctx, f, area);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect()
    }

    fn chat_pane_rows(ctx: &mut TuiContext, w: u16, h: u16) -> Vec<String> {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_chat_pane(&AppState::Ready, ctx, f, area);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// A Markdown `---` renders as a full-width rule, not literal dashes.
    #[test]
    fn chat_pane_renders_markdown_rule_not_literal_dashes() {
        let (mut ctx, _r, _e) = TuiContext::test_context();
        ctx.append_assistant_message("above the rule\n---\nbelow the rule")
            .unwrap();
        let text = chat_pane_buffer_text(&mut ctx, 50, 12);
        assert!(text.contains("above the rule"));
        assert!(text.contains("below the rule"));
        assert!(
            text.contains(&"─".repeat(10)),
            "the --- line must render as a run of rule characters"
        );
        assert!(
            !text.contains("---"),
            "no literal triple-dash should remain"
        );
    }

    /// Turns are separated by a blank line so a "You:" never butts up against
    /// the reply above it.
    #[test]
    fn chat_pane_separates_turns_with_a_blank_line() {
        let (mut ctx, _r, _e) = TuiContext::test_context();
        ctx.append_user_message("first question").unwrap();
        ctx.append_assistant_message("first answer").unwrap();
        let rows = chat_pane_rows(&mut ctx, 60, 12);
        let q = rows
            .iter()
            .position(|r| r.contains("first question"))
            .unwrap();
        let a = rows
            .iter()
            .position(|r| r.contains("first answer"))
            .unwrap();
        assert!(a > q + 1, "expected a blank row between the two turns");
        // A separator row carries only the box borders and spaces — no text.
        let is_blank = |r: &String| r.chars().all(|c| c == '│' || c == ' ');
        assert!(
            rows[q + 1..a].iter().any(is_blank),
            "a blank separator row must sit between the turns"
        );
    }

    /// Regression: the mouse wheel must scroll the chat pane, not fall through
    /// to the composer. Before mouse capture, the terminal turned the wheel into
    /// Up/Down arrows, which recalled history into the input buffer.
    #[tokio::test]
    async fn mouse_wheel_scrolls_chat_not_composer() {
        use crossterm::event::{MouseEvent, MouseEventKind};
        let (ctx, _r, _e) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle_event(wheel(MouseEventKind::ScrollUp))
            .await
            .unwrap();
        app.handle_event(wheel(MouseEventKind::ScrollUp))
            .await
            .unwrap();
        assert_eq!(
            app.context.scroll_offset, 6,
            "two wheel-ups scroll six lines back"
        );
        app.handle_event(wheel(MouseEventKind::ScrollDown))
            .await
            .unwrap();
        assert_eq!(
            app.context.scroll_offset, 3,
            "wheel-down scrolls toward the bottom"
        );
        assert_eq!(
            app.context.input_buffer, "",
            "the wheel must not touch the composer"
        );
    }

    /// The Windows-copy flow, restored: drag highlights, Ctrl+C with a
    /// selection copies (and must NOT quit or cancel anything).
    #[tokio::test]
    async fn drag_then_ctrl_c_copies_and_does_not_quit() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut ctx, _r, _e) = TuiContext::test_context();
        ctx.append_user_message("question one").unwrap();
        ctx.append_assistant_message("answer line").unwrap();
        // Render once so the mouse handler has the pane rect, window start
        // and provenance the real event loop would have stashed.
        let _ = chat_pane_rows(&mut ctx, 60, 12);
        let mut app = make_app_with_context(ctx);
        let m = |kind, row| {
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        // Row 1 = first display line ("You: question one"), row 3 = the reply.
        app.handle_event(m(MouseEventKind::Down(MouseButton::Left), 1))
            .await
            .unwrap();
        app.handle_event(m(MouseEventKind::Drag(MouseButton::Left), 3))
            .await
            .unwrap();
        assert!(
            app.context.selection.is_some(),
            "drag must arm a selection over the transcript"
        );

        let res = app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(
            res,
            EventResult::Continue,
            "Ctrl+C with a selection copies — it must not quit"
        );
        assert!(
            app.context.selection.is_none(),
            "the copy consumes the selection"
        );
        let notice = app.context.status_notice.clone().unwrap_or_default();
        assert!(
            notice.contains("copied"),
            "the status line must confirm the copy, got: {notice}"
        );
    }

    /// Pin BOTH legacy Ctrl+C arms: quit while Ready, cancel while Streaming
    /// (the second is also pinned by the older cancel test) — the selection
    /// branch must not disturb either.
    #[tokio::test]
    async fn ctrl_c_without_selection_in_ready_still_quits() {
        let (ctx, _r, _e) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Ready;
        let res = app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(
            res,
            EventResult::Quit,
            "no selection + Ready must still quit"
        );
    }

    /// An operator with a highlight active must never cancel their running
    /// turn by copying — the Hermes Ctrl+C-conflict lesson.
    #[tokio::test]
    async fn ctrl_c_with_selection_while_streaming_copies_not_cancels() {
        let (mut ctx, mut req_rx, _e) = TuiContext::test_context();
        ctx.append_user_message("q").unwrap();
        ctx.selection = Some(crate::tui::selection::Selection {
            anchor: (0, 0),
            head: (0, 0),
        });
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: Vec::new(),
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };

        let res = app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(res, EventResult::Continue);
        assert!(app.context.selection.is_none());
        assert!(
            req_rx.try_recv().is_err(),
            "copy must not send a Cancel to the agent"
        );
        if let AppState::Streaming { cancelling, .. } = &app.state {
            assert!(!cancelling, "the turn must keep running untouched");
        } else {
            panic!("state must remain Streaming");
        }
    }

    /// Esc ordering: the first Esc drops the highlight, the second cancels
    /// the running turn — selection sits above streaming-cancel, below every
    /// modal.
    #[tokio::test]
    async fn esc_clears_selection_before_cancelling_the_turn() {
        let (mut ctx, mut req_rx, _e) = TuiContext::test_context();
        ctx.append_user_message("q").unwrap();
        ctx.selection = Some(crate::tui::selection::Selection {
            anchor: (0, 0),
            head: (0, 0),
        });
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: Vec::new(),
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(
            app.context.selection.is_none(),
            "first Esc clears the highlight"
        );
        assert!(
            req_rx.try_recv().is_err(),
            "first Esc must not cancel the turn"
        );

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(
            matches!(req_rx.try_recv(), Ok(TurnRequest::Cancel)),
            "second Esc cancels the turn as before"
        );
    }

    /// A plain click (Down+Up, no drag) clears the highlight — the Windows
    /// model again.
    #[tokio::test]
    async fn plain_click_clears_selection() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut ctx, _r, _e) = TuiContext::test_context();
        ctx.append_user_message("hello").unwrap();
        let _ = chat_pane_rows(&mut ctx, 60, 12);
        ctx.selection = Some(crate::tui::selection::Selection {
            anchor: (0, 0),
            head: (0, 0),
        });
        let mut app = make_app_with_context(ctx);
        let m = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle_event(m(MouseEventKind::Down(MouseButton::Left)))
            .await
            .unwrap();
        app.handle_event(m(MouseEventKind::Up(MouseButton::Left)))
            .await
            .unwrap();
        assert!(app.context.selection.is_none(), "a click deselects");
    }

    /// Compaction replaces the transcript wholesale — anchors into the old
    /// history must not survive it.
    #[tokio::test]
    async fn compaction_clears_the_selection() {
        let (mut ctx, _r, _e) = TuiContext::test_context();
        ctx.append_user_message("old question").unwrap();
        ctx.append_assistant_message("old answer").unwrap();
        ctx.selection = Some(crate::tui::selection::Selection {
            anchor: (0, 0),
            head: (1, 0),
        });
        let mut app = make_app_with_context(ctx);
        app.handle_agent_event(AgentEvent::CompactionComplete {
            summary: "a summary".into(),
            original_count: 2,
            kept_count: 0,
            keep_last: 0,
        });
        assert!(
            app.context.selection.is_none(),
            "history replacement must drop the selection"
        );
    }

    /// The highlight actually paints: the selected row gets the selection
    /// background, an unselected row does not.
    #[test]
    fn selection_paints_background_on_selected_rows_only() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let (mut ctx, _r, _e) = TuiContext::test_context();
        ctx.append_user_message("picked").unwrap();
        ctx.append_assistant_message("not picked").unwrap();
        ctx.selection = Some(crate::tui::selection::Selection {
            anchor: (0, 0),
            head: (0, 0),
        });
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|f| {
            let area = f.area();
            render_chat_pane(&AppState::Ready, &mut ctx, f, area);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        // Row 1 = "You: picked" (selected); row 3 = the reply (not selected).
        assert_eq!(
            buf[(2u16, 1u16)].style().bg,
            Some(SELECTION_BG),
            "selected row must carry the selection background"
        );
        assert_ne!(
            buf[(2u16, 3u16)].style().bg,
            Some(SELECTION_BG),
            "unselected row must not"
        );
    }

    /// Regression: a single message taller than the pane used to render the
    /// chat area completely blank (ratatui `List` draws nothing when the
    /// selected item is taller than the viewport). The flat line-window render
    /// must show the BOTTOM of the long message (stick-to-bottom).
    #[test]
    fn chat_pane_shows_bottom_of_a_message_taller_than_the_pane() {
        let (mut ctx, _r, _e) = TuiContext::test_context();
        let long: String = (1..=200)
            .map(|i| format!("answer line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        ctx.append_assistant_message(&long).unwrap();
        let text = chat_pane_buffer_text(&mut ctx, 80, 20);
        assert!(
            text.contains("answer line 200"),
            "the newest (bottom) line must be visible, not a blank pane"
        );
        assert!(
            !text.contains("answer line 1 "),
            "the top of a too-tall message should be scrolled off, not shown"
        );
    }

    /// Regression: a reply wider than the pane must WRAP (reflow) into the pane,
    /// not be clipped off the right edge.
    #[test]
    fn chat_pane_wraps_a_line_wider_than_the_pane() {
        let (mut ctx, _r, _e) = TuiContext::test_context();
        // One long line, no newlines, far wider than a 40-col pane.
        let wide = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike";
        ctx.append_assistant_message(wide).unwrap();
        let text = chat_pane_buffer_text(&mut ctx, 40, 12);
        // Every word survives (wrapped onto later rows), including the last one
        // that a non-wrapping renderer would have clipped.
        assert!(text.contains("alpha"), "first word visible");
        assert!(
            text.contains("mike"),
            "last word must wrap into view, not clip"
        );
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
            TurnRequest::Submit {
                text,
                conversation_id,
            } => {
                assert_eq!(text, "hello");
                // Submitting bound a session (persist-before-send), so the
                // request must carry that session's memory scope — this is
                // what keeps TUI recall out of other conversations' rows.
                let sid = app
                    .context
                    .session_id
                    .clone()
                    .expect("submit binds a session");
                assert_eq!(conversation_id, Some(format!("tui:{sid}")));
            }
            TurnRequest::Cancel => panic!("expected Submit, got Cancel"),
            TurnRequest::Reload { .. } => panic!("expected Submit, got Reload"),
            TurnRequest::Compact { .. } => panic!("expected Submit, got Compact"),
            TurnRequest::SetHistory(_) => panic!("expected Submit, got SetHistory"),
        }
    }

    /// End-to-end handler wiring: pressing Up on the last visual row of a
    /// soft-wrapped single line moves the caret up one visual row (using the
    /// width captured in `last_composer_width`) instead of falling through to
    /// history. With no history in the test context, the old logical-line
    /// behaviour would have left the caret untouched, so `after < before`
    /// proves both the new movement and the handler plumbing.
    #[tokio::test]
    async fn arrow_up_moves_caret_across_wrapped_rows_via_handler() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let (mut ctx, _req_rx, _events_tx) = TuiContext::test_context();
        // 30 chars, no `\n`, wraps to 3 rows at inner_w 12 (10 on row 0 after
        // the 2-cell prefix, then 12, then 8).
        ctx.input_buffer = "abcdefghijklmnopqrstuvwxyz0123".chars().collect();
        ctx.last_composer_width = 12;
        ctx.cursor_pos = ctx.input_buffer.chars().count(); // end → last row
        let mut app = make_app_with_context(ctx);
        app.state = AppState::Ready;

        let before = app.context.cursor_pos;
        let end_row = layout_composer(&app.context.input_buffer, before, 12).caret_row;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await
            .unwrap();
        let after = app.context.cursor_pos;
        assert!(
            after < before,
            "Up should move the caret up: {before} -> {after}"
        );
        let new_row = layout_composer(&app.context.input_buffer, after, 12).caret_row;
        assert_eq!(new_row, end_row - 1, "Up should land one visual row above");
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
            TurnRequest::Submit { text, .. } => assert_eq!(text, "queued"),
            TurnRequest::Cancel => panic!("expected Submit, got Cancel"),
            TurnRequest::Reload { .. } => panic!("expected Submit, got Reload"),
            TurnRequest::Compact { .. } => panic!("expected Submit, got Compact"),
            TurnRequest::SetHistory(_) => panic!("expected Submit, got SetHistory"),
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

    use super::test_support::app_with_context as make_app_with_context;
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
        // Ctrl chords the composer does NOT implement (a/e/u/w/k ARE handled and
        // tested separately). Each must be ignored, never inserted as a letter.
        for c in ['b', 'f', 'l', 'n', 'o', 'p', 'r', 't', 'x', 'z'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
                .await
                .unwrap();
        }

        assert_eq!(app.context.input_buffer, "hello");
    }

    /// The implemented readline chords must act (not type their letter) through
    /// the real key handler: Ctrl+W deletes the last word, Ctrl+U then clears
    /// what remains of the line.
    #[tokio::test]
    async fn implemented_ctrl_chords_edit_through_handler() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        for c in "hello world".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .await
                .unwrap();
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(
            app.context.input_buffer, "hello ",
            "Ctrl+W deletes the last word"
        );
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert_eq!(app.context.input_buffer, "", "Ctrl+U clears to line start");
        assert_eq!(app.context.cursor_pos, 0);
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

    use super::test_support::app_with_context as make_app_with_context;
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

    /// "This session" on `/usage` is the sum of every turn, not the last one:
    /// three turns of 11/7/18 total 33/21/54.
    #[test]
    fn usage_events_sum_across_turns_in_the_session() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        for _ in 0..3 {
            app.handle_agent_event(AgentEvent::Usage(crate::cost::TokenUsage::new(
                "mock-model",
                11,
                7,
                None,
            )));
        }
        let total = app.context.token_usage.as_ref().expect("reported");
        assert_eq!(total.prompt_tokens, 33);
        assert_eq!(total.completion_tokens, 21);
        assert_eq!(total.total_tokens, 54);
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
mod recalled_memory_tests {
    use super::*;

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    #[test]
    fn names_the_recalled_memories() {
        assert_eq!(
            render_recalled_memories(&keys(&["user_lang", "project"])),
            "↺ recalled 2 memories: user_lang, project"
        );
    }

    #[test]
    fn one_memory_reads_as_singular() {
        assert_eq!(
            render_recalled_memories(&keys(&["user_lang"])),
            "↺ recalled 1 memory: user_lang"
        );
    }

    /// The count stays honest when the names are abbreviated — an operator
    /// reading "3 memories" while seeing two names would mistrust both numbers.
    #[test]
    fn summarises_the_tail_without_losing_the_count() {
        let line = render_recalled_memories(&keys(&["a", "b", "c", "d", "e"]));
        assert_eq!(line, "↺ recalled 5 memories: a, b, c, +2 more");
    }

    /// A uuid names nothing to the reader. Counting the turn entries the retired
    /// auto-save wrote says more in less width than three lines of hex.
    #[test]
    fn autosaved_turns_are_counted_not_named() {
        let line = render_recalled_memories(&keys(&[
            "user_msg_23f4b294-d91b-4ba4-9fd5-a902a78f3a82",
            "user_msg_9bd37f53-4f36-4819-972b-21cf335e6280",
            "deploy_window",
        ]));
        assert_eq!(
            line,
            "↺ recalled 3 memories: deploy_window, 2 from this conversation"
        );
        assert!(!line.contains("23f4b294"), "uuid leaked into the line");
    }

    #[test]
    fn all_autosaved_reads_as_a_count_alone() {
        let line = render_recalled_memories(&keys(&[
            "user_msg_23f4b294-d91b-4ba4-9fd5-a902a78f3a82",
            "user_msg_9bd37f53-4f36-4819-972b-21cf335e6280",
        ]));
        assert_eq!(line, "↺ recalled 2 memories: 2 from this conversation");
    }

    #[test]
    fn exactly_the_shown_limit_has_no_tail() {
        let line = render_recalled_memories(&keys(&["a", "b", "c"]));
        assert_eq!(line, "↺ recalled 3 memories: a, b, c");
        assert!(!line.contains("more"), "no tail to summarise: {line}");
    }

    /// A key ending in something uuid-shaped is generated; a chosen name that
    /// merely contains underscores is not.
    #[test]
    fn only_uuid_tails_count_as_generated() {
        assert!(crate::memory::is_autosave_key(
            "user_msg_23f4b294-d91b-4ba4-9fd5-a902a78f3a82"
        ));
        assert!(!crate::memory::is_autosave_key("deploy_window"));
        assert!(!crate::memory::is_autosave_key("user_lang"));
        assert!(!crate::memory::is_autosave_key("plan_2026_08_06"));
    }
}

#[cfg(test)]
mod narrow_terminal_tests {
    use super::*;

    fn seg(text: &'static str) -> Vec<Span<'static>> {
        vec![Span::raw(text)]
    }

    /// A `Paragraph` clips at the character, so an overflowing status line lost
    /// the end of its last field — "Smart" rendered as "S" at 60 columns.
    #[test]
    fn fit_segments_drops_whole_segments_by_rank() {
        let required = seg("MODEL");
        let optional = vec![
            (2, seg("|tokens")),
            (1, seg("|msgs")),
            (3, seg("|age")),
            (0, seg("|SAFE")),
        ];

        let wide: String = fit_segments(required.clone(), optional.clone(), 100)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(wide, "MODEL|tokens|msgs|age|SAFE");

        // Tight: highest ranks go first, and the rest keep their order.
        let tight: String = fit_segments(required.clone(), optional.clone(), 16)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            tight, "MODEL|msgs|SAFE",
            "rank 3 then 2 should go, in order"
        );

        // Down to a genuinely cramped terminal, the safety signal is the last
        // optional field standing. (Below this nothing fits, which is not a
        // terminal anyone has.)
        let tiny: String = fit_segments(required, optional, 11)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(
            tiny, "MODEL|SAFE",
            "the safety signal must be the last to go"
        );
    }

    /// Rank must not double as position: deciding what to sacrifice cannot
    /// reorder what survives.
    #[test]
    fn fit_segments_preserves_declaration_order() {
        let out: String = fit_segments(
            seg("A"),
            vec![(9, seg("B")), (0, seg("C")), (5, seg("D"))],
            100,
        )
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
        assert_eq!(out, "ABCD", "display order follows declaration, not rank");
    }

    /// The hint used to be clipped mid-phrase: at 80 columns it ended
    /// "· Ctrl+C" with an unclosed parenthesis and no "exit" — the one thing a
    /// stuck operator needs was the first to go.
    #[test]
    fn composer_placeholder_degrades_without_dangling_text() {
        for width in [40_u16, 56, 64, 72, 80, 100, 160] {
            let text = composer_body("", width);
            let rendered: String = text.lines[0]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect();

            assert!(
                rendered.contains("Type a message"),
                "w={width}: placeholder missing: {rendered}"
            );
            assert_eq!(
                rendered.matches('(').count(),
                rendered.matches(')').count(),
                "w={width}: unbalanced parenthesis: {rendered}"
            );
            if rendered.contains('(') {
                assert!(
                    rendered.contains("Ctrl+C exit"),
                    "w={width}: the exit hint must outlive the others: {rendered}"
                );
            }
        }
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::tui::async_bridge::TurnRequest;
    use crate::tui::context::TuiContext;

    use super::test_support::app_with_context as make_app_with_context;
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
        // The agent is reseeded with the turns before the retried one, then
        // the same text is submitted.
        let reseed = req_rx.recv().await.expect("reseed should dispatch");
        assert!(
            matches!(reseed, TurnRequest::SetHistory(ref t) if t.is_empty()),
            "expected SetHistory with no turns before the retried one, got {reseed:?}"
        );
        let req = req_rx.recv().await.expect("resubmit should dispatch");
        match req {
            TurnRequest::Submit { text, .. } => assert_eq!(text, "previous prompt"),
            TurnRequest::Cancel => panic!("expected Submit, got Cancel"),
            TurnRequest::Reload { .. } => panic!("expected Submit, got Reload"),
            TurnRequest::Compact { .. } => panic!("expected Submit, got Compact"),
            TurnRequest::SetHistory(_) => panic!("expected Submit, got SetHistory"),
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
        let last_msg = app.context.messages.last().expect("note message");
        assert_eq!(last_msg.role, "note");
        assert!(
            last_msg.content.contains("No previous response"),
            "got {:?}",
            last_msg.content
        );
    }
}

/// Shift+Tab cycles the autonomy preset, and one of that ladder's rungs is
/// "no prompts". The guard on that keybinding is therefore a security control,
/// and until this module existed nothing tested it: the arm lives in
/// `handle_key`, and every test helper in this file pointed the profile at the
/// operator's real directory, so driving the cycle would have rewritten the
/// machine's own policy files.
///
/// The oracle throughout is the preset actually written to disk under a
/// `TempDir`, not a flag or a mock — `cycle_autonomy_preset`'s whole effect is
/// that write.
#[cfg(test)]
mod session_history_tests {
    use super::*;
    use crate::sessions::{Message, SessionStore};
    use crate::tui::async_bridge::TurnRequest;
    use crate::tui::context::TuiContext;
    use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};
    use std::sync::{Arc, Mutex};

    use super::test_support::{app_with_context as make_app_with_context, app_with_profile_root};

    /// Stores `pairs` as one session in `store`, returning its id.
    fn store_session(store: &SessionStore, pairs: &[(&str, &str)]) -> String {
        store_session_with_model(store, "mock-model", pairs)
    }

    /// As [`store_session`], under a recorded `model`: the label the status bar
    /// held when that session opened.
    fn store_session_with_model(
        store: &SessionStore,
        model: &str,
        pairs: &[(&str, &str)],
    ) -> String {
        let session = store.new_session(model, "tui").expect("session");
        for (role, content) in pairs {
            store
                .append_message(&Message {
                    id: 0,
                    session_id: session.id.clone(),
                    role: (*role).to_string(),
                    content: (*content).to_string(),
                    tool_calls: None,
                    timestamp: chrono::Utc::now().timestamp(),
                })
                .expect("message");
        }
        session.id
    }

    /// Opens the session picker on `key` and presses Enter, as the user does.
    async fn pick_session(app: &mut TuiApp, key: &str) {
        let item = ListPickerItem {
            key: key.to_string(),
            primary: key.to_string(),
            secondary: String::new(),
            disabled: false,
        };
        app.list_picker = Some(ListPicker::new(
            ListPickerKind::Session,
            "Resume Session",
            vec![item],
            Some(key),
            "No past sessions to resume.",
        ));
        app.dispatch_list_picker_selection().await;
    }

    /// `/undo` removes the exchange from the store as well as the screen, and
    /// the agent is reseeded so the undone turns are not sent.
    #[tokio::test]
    async fn undo_removes_the_exchange_from_the_store_and_the_agent() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("kept question").unwrap();
        app.context.append_assistant_message("kept answer").unwrap();
        app.context.append_user_message("undone question").unwrap();
        app.context
            .append_assistant_message("undone answer")
            .unwrap();
        let sid = app.context.session_id.clone().expect("bound session");

        app.handle_command("undo").await.unwrap();

        // The store keeps the kept exchange, and the screen note that reports
        // the removal (a note, not a turn). The undone rows are gone.
        let stored: Vec<(String, String)> = app
            .context
            .session_store
            .get_messages(&sid)
            .unwrap()
            .into_iter()
            .map(|m| (m.role, m.content))
            .collect();
        assert_eq!(
            stored,
            [
                ("user".to_string(), "kept question".to_string()),
                ("assistant".to_string(), "kept answer".to_string()),
                ("note".to_string(), "Removed 2 message(s).".to_string()),
            ],
            "the DB keeps the kept exchange and the note"
        );
        match req_rx.try_recv() {
            Ok(TurnRequest::SetHistory(turns)) => assert_eq!(
                turns,
                [
                    ("user".to_string(), "kept question".to_string()),
                    ("assistant".to_string(), "kept answer".to_string()),
                ]
            ),
            other => panic!("expected SetHistory, got {other:?}"),
        }
    }

    /// `/retry` keeps the user turn once and drops the old reply, from the
    /// screen, the store, and the agent. The retried text is submitted again,
    /// so the agent's history must not already hold it.
    #[tokio::test]
    async fn retry_drops_the_old_reply_from_the_store_and_the_agent() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("earlier question").unwrap();
        app.context
            .append_assistant_message("earlier answer")
            .unwrap();
        app.context.append_user_message("the question").unwrap();
        app.context.append_assistant_message("old reply").unwrap();
        let sid = app.context.session_id.clone().expect("bound session");
        app.state = AppState::Ready;

        app.handle_command("retry").await.unwrap();

        let stored: Vec<(String, String)> = app
            .context
            .session_store
            .get_messages(&sid)
            .unwrap()
            .into_iter()
            .map(|m| (m.role, m.content))
            .collect();
        assert_eq!(
            stored,
            [
                ("user".to_string(), "earlier question".to_string()),
                ("assistant".to_string(), "earlier answer".to_string()),
                ("user".to_string(), "the question".to_string()),
            ],
            "the DB keeps the retried user turn once and not the old reply"
        );
        match req_rx.try_recv() {
            Ok(TurnRequest::SetHistory(turns)) => assert_eq!(
                turns,
                [
                    ("user".to_string(), "earlier question".to_string()),
                    ("assistant".to_string(), "earlier answer".to_string()),
                ],
                "the agent holds the history before the retried turn"
            ),
            other => panic!("expected SetHistory first, got {other:?}"),
        }
        match req_rx.try_recv() {
            Ok(TurnRequest::Submit { text, .. }) => assert_eq!(text, "the question"),
            other => panic!("expected the retried Submit, got {other:?}"),
        }
    }

    /// `/undo` after a failed turn removes the failed question and its note,
    /// and keeps the exchange before it.
    #[tokio::test]
    async fn undo_after_a_failed_turn_removes_the_question_and_the_note() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("Q1").unwrap();
        app.context.append_assistant_message("A1").unwrap();
        app.context.append_user_message("Q2").unwrap();
        app.context
            .append_note_message("✗ provider timed out")
            .unwrap();
        let sid = app.context.session_id.clone().expect("bound session");

        app.handle_command("undo").await.unwrap();

        let stored: Vec<String> = app
            .context
            .session_store
            .get_messages(&sid)
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(stored[..2], ["Q1", "A1"], "the earlier exchange stays");
        assert_eq!(stored[2], "Removed 2 message(s).");
        assert_eq!(stored.len(), 3, "Q2 and the failed note are gone");
        match req_rx.try_recv() {
            Ok(TurnRequest::SetHistory(turns)) => assert_eq!(turns.len(), 2),
            other => panic!("expected SetHistory, got {other:?}"),
        }
    }

    /// After a compaction, `/undo` must touch only the undone exchange in the
    /// store. The kept turns carried pre-compaction ids into the screen; the
    /// store re-numbered them, so a stale id used to match every stored row
    /// and delete the whole session.
    #[tokio::test]
    async fn undo_after_compaction_removes_only_the_undone_exchange() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("old Q").unwrap();
        app.context.append_assistant_message("old A").unwrap();
        app.context.append_user_message("kept Q").unwrap();
        app.context.append_assistant_message("kept A").unwrap();
        app.context.append_user_message("undone Q").unwrap();
        app.context.append_assistant_message("undone A").unwrap();
        let sid = app.context.session_id.clone().expect("bound session");
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };
        app.handle_agent_event(AgentEvent::CompactionComplete {
            summary: "we talked".into(),
            original_count: 6,
            kept_count: 4,
            keep_last: 2,
        });
        req_rx.try_recv().ok();

        app.handle_command("undo").await.unwrap();

        let stored: Vec<String> = app
            .context
            .session_store
            .get_messages(&sid)
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        let screen: Vec<String> = app
            .context
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(
            stored, screen,
            "the store holds exactly what the screen shows after /undo"
        );
        assert!(
            stored.iter().any(|c| c == "kept A"),
            "the kept exchange survives the undo: {stored:?}"
        );
    }

    /// After a compaction, `/retry` cuts only the reply after the last user
    /// turn, and the rows before it stay in the store.
    #[tokio::test]
    async fn retry_after_compaction_cuts_only_the_last_reply() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("old Q").unwrap();
        app.context.append_assistant_message("old A").unwrap();
        app.context.append_user_message("kept Q").unwrap();
        app.context.append_assistant_message("kept A").unwrap();
        app.context.append_user_message("last Q").unwrap();
        app.context.append_assistant_message("last A").unwrap();
        let sid = app.context.session_id.clone().expect("bound session");
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };
        app.handle_agent_event(AgentEvent::CompactionComplete {
            summary: "we talked".into(),
            original_count: 6,
            kept_count: 4,
            keep_last: 2,
        });
        req_rx.try_recv().ok();
        app.state = AppState::Ready;

        app.handle_command("retry").await.unwrap();

        let stored: Vec<String> = app
            .context
            .session_store
            .get_messages(&sid)
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        let screen: Vec<String> = app
            .context
            .messages
            .iter()
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(stored, screen, "store and screen agree after /retry");
        assert!(
            stored.iter().any(|c| c == "kept A"),
            "the kept exchange survives the retry: {stored:?}"
        );
        assert!(
            !stored.iter().any(|c| c == "last A"),
            "the reply to the retried question is gone: {stored:?}"
        );
    }

    /// The reseed is not lost when the bridge is busy: with the channel full,
    /// `/new` still reaches the agent before the next prompt, so the next
    /// request never carries the session that was just ended.
    #[tokio::test]
    async fn new_reseed_survives_a_full_request_channel() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("old question").unwrap();
        app.context.append_assistant_message("old answer").unwrap();
        // Fill the bridge (capacity 4 in the test context).
        for i in 0..4 {
            app.context
                .req_tx
                .try_send(TurnRequest::Cancel)
                .unwrap_or_else(|_| panic!("fill slot {i}"));
        }

        // The actor drains the bridge while the command runs, so the held
        // reseed can go out. Collect what arrives, in order.
        let drain = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(req) = req_rx.recv().await {
                let is_reseed = matches!(req, TurnRequest::SetHistory(_));
                seen.push(req);
                if is_reseed {
                    break;
                }
            }
            seen
        });

        app.handle_command("new").await.unwrap();

        // Bounded: a reseed that never arrives must fail the test, not hang it.
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), drain)
            .await
            .expect("the drainer should finish; a lost reseed never arrives")
            .unwrap();
        let reseed_at = seen
            .iter()
            .position(|r| matches!(r, TurnRequest::SetHistory(_)))
            .expect("the reseed was lost");
        assert_eq!(
            reseed_at, 4,
            "the reseed follows the four fillers, not before them"
        );
        assert!(app.context.last_error.is_none());
    }

    /// A reseed held back must not overwrite a newer one that got through: if
    /// room opens and a later reseed is sent, the older held one is dropped,
    /// so the agent ends with the newest history.
    #[tokio::test]
    async fn a_held_older_reseed_does_not_overwrite_a_newer_one() {
        let (mut ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        // Fill the bridge so the next reseed is held.
        for _ in 0..4 {
            ctx.req_tx.try_send(TurnRequest::Cancel).unwrap();
        }
        ctx.reseed_agent(&[("user".to_string(), "older".to_string())]);
        // Room opens, and a newer reseed gets through first.
        for _ in 0..4 {
            assert!(matches!(req_rx.try_recv(), Ok(TurnRequest::Cancel)));
        }
        ctx.reseed_agent(&[("user".to_string(), "newer".to_string())]);
        ctx.flush_pending_reseed().await;

        match req_rx.try_recv() {
            Ok(TurnRequest::SetHistory(turns)) => assert_eq!(
                turns,
                [("user".to_string(), "newer".to_string())],
                "the agent must end with the newest history"
            ),
            other => panic!("expected the newer SetHistory, got {other:?}"),
        }
        assert!(
            req_rx.try_recv().is_err(),
            "the older held reseed must not be sent after the newer one"
        );
    }

    /// A `/compress` issued while a reseed is held reaches the agent after the
    /// reseed, so the compaction runs on the session the screen shows and the
    /// later reseed cannot overwrite its result.
    #[tokio::test]
    async fn compress_issued_while_a_reseed_is_held_follows_the_reseed() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("Q1").unwrap();
        app.context.append_assistant_message("A1").unwrap();
        for i in 0..15 {
            app.context.messages.push(crate::sessions::Message {
                id: 0,
                session_id: app.context.session_id.clone().unwrap_or_default(),
                role: if i % 2 == 0 { "user" } else { "assistant" }.into(),
                content: format!("msg-{i}"),
                tool_calls: None,
                timestamp: 0,
            });
        }
        // Fill the bridge, then hold a reseed behind it.
        for _ in 0..4 {
            app.context.req_tx.try_send(TurnRequest::Cancel).unwrap();
        }
        app.context
            .reseed_agent(&[("user".to_string(), "older".to_string())]);
        assert!(app.context.pending_reseed.is_some(), "the reseed is held");
        // One slot frees, so the compaction can be sent but the held reseed
        // still has to wait for its own flush point.
        assert!(matches!(req_rx.recv().await, Some(TurnRequest::Cancel)));

        // Collect everything the agent receives, in order, until the
        // channel goes quiet: a reseed sent after the compaction is seen too.
        let drain = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Ok(Some(req)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), req_rx.recv()).await
            {
                seen.push(req);
            }
            seen
        });

        app.handle_command("compress").await.unwrap();

        let seen = drain.await.unwrap();
        let reseed_at = seen
            .iter()
            .position(|r| matches!(r, TurnRequest::SetHistory(_)))
            .expect("the held reseed reaches the agent");
        let compact_at = seen
            .iter()
            .position(|r| matches!(r, TurnRequest::Compact { .. }))
            .expect("the compaction reaches the agent");
        assert!(
            reseed_at < compact_at,
            "the reseed must precede the compaction: {seen:?}"
        );
    }

    /// A reseed the agent can no longer receive (the actor has gone) is
    /// reported in the status line, not dropped silently.
    #[tokio::test]
    async fn reseed_to_a_closed_agent_is_reported_not_dropped() {
        let (ctx, req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("old question").unwrap();
        app.context.append_assistant_message("old answer").unwrap();
        drop(req_rx);

        app.handle_command("new").await.unwrap();

        let err = app.context.last_error.as_deref().unwrap_or("");
        assert!(
            err.contains("history not reseeded"),
            "a dead agent must be surfaced, got {err:?}"
        );
    }

    /// A failed first turn leaves `[user, note]`. `/retry` re-asks that
    /// question: the note is not a reply, so the retry is not refused.
    #[tokio::test]
    async fn retry_after_a_failed_first_turn_resubmits_the_question() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("the question").unwrap();
        app.context
            .append_note_message("✗ provider timed out")
            .unwrap();
        app.state = AppState::Ready;

        app.handle_command("retry").await.unwrap();

        match req_rx.try_recv() {
            Ok(TurnRequest::SetHistory(turns)) => assert!(turns.is_empty(), "{turns:?}"),
            other => panic!("expected SetHistory, got {other:?}"),
        }
        match req_rx.try_recv() {
            Ok(TurnRequest::Submit { text, .. }) => assert_eq!(text, "the question"),
            other => panic!("expected the retried Submit, got {other:?}"),
        }
        assert!(app.context.last_error.is_none());
    }

    /// After an earlier exchange, a failed turn leaves `[Q1, A1, Q2, note]`.
    /// `/retry` re-asks Q2 and leaves the real reply A1 untouched on screen,
    /// in the store, and in the next request.
    #[tokio::test]
    async fn retry_after_a_failed_turn_keeps_the_earlier_exchange() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("Q1").unwrap();
        app.context.append_assistant_message("A1").unwrap();
        app.context.append_user_message("Q2").unwrap();
        app.context
            .append_note_message("✗ provider timed out")
            .unwrap();
        let sid = app.context.session_id.clone().expect("bound session");
        app.state = AppState::Ready;

        app.handle_command("retry").await.unwrap();

        let screen: Vec<(&str, &str)> = app
            .context
            .messages
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect();
        assert_eq!(
            screen,
            [("user", "Q1"), ("assistant", "A1"), ("user", "Q2")]
        );
        let stored: Vec<(String, String)> = app
            .context
            .session_store
            .get_messages(&sid)
            .unwrap()
            .into_iter()
            .map(|m| (m.role, m.content))
            .collect();
        assert_eq!(
            stored,
            [
                ("user".to_string(), "Q1".to_string()),
                ("assistant".to_string(), "A1".to_string()),
                ("user".to_string(), "Q2".to_string()),
            ],
            "the DB keeps A1 and drops the failed note"
        );
        match req_rx.try_recv() {
            Ok(TurnRequest::SetHistory(turns)) => assert_eq!(
                turns,
                [
                    ("user".to_string(), "Q1".to_string()),
                    ("assistant".to_string(), "A1".to_string()),
                ],
                "the next request carries A1 and not Q2"
            ),
            other => panic!("expected SetHistory, got {other:?}"),
        }
        match req_rx.try_recv() {
            Ok(TurnRequest::Submit { text, .. }) => assert_eq!(text, "Q2"),
            other => panic!("expected the retried Submit, got {other:?}"),
        }
    }

    /// A cancelled turn leaves `[Q1, A1, Q2]` with no reply to Q2. `/retry`
    /// re-asks Q2 and keeps A1.
    #[tokio::test]
    async fn retry_after_a_cancelled_turn_resubmits_the_unanswered_question() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("Q1").unwrap();
        app.context.append_assistant_message("A1").unwrap();
        app.context.append_user_message("Q2").unwrap();
        let sid = app.context.session_id.clone().expect("bound session");
        app.state = AppState::Ready;

        app.handle_command("retry").await.unwrap();

        let stored = app.context.session_store.get_messages(&sid).unwrap();
        assert_eq!(
            stored
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>(),
            ["Q1", "A1", "Q2"],
            "nothing is cut when no reply to Q2 exists"
        );
        match req_rx.try_recv() {
            Ok(TurnRequest::SetHistory(turns)) => assert_eq!(turns.len(), 2),
            other => panic!("expected SetHistory, got {other:?}"),
        }
        match req_rx.try_recv() {
            Ok(TurnRequest::Submit { text, .. }) => assert_eq!(text, "Q2"),
            other => panic!("expected the retried Submit, got {other:?}"),
        }
    }

    /// A compacted session, picked from the list, sends its summary and the
    /// real system prompt, and the notes around it do not reach the model.
    #[tokio::test]
    async fn picking_a_compacted_session_sends_the_summary_as_context() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        let summary = "[Compacted summary of earlier conversation]\n\nwe chose sqlite\n\n[End compacted summary]";
        let session = store_session(
            &app.context.session_store,
            &[
                ("note", "Resumed session 12176d39 (4 messages)"),
                ("system", summary),
                ("user", "the next question"),
                ("assistant", "the next answer"),
            ],
        );
        app.state = AppState::Ready;

        pick_session(&mut app, &session).await;

        match req_rx.try_recv() {
            Ok(TurnRequest::SetHistory(turns)) => assert_eq!(
                turns,
                [
                    ("user".to_string(), summary.to_string()),
                    ("user".to_string(), "the next question".to_string()),
                    ("assistant".to_string(), "the next answer".to_string()),
                ],
                "the summary is context; the resume note is not sent"
            ),
            other => panic!("expected SetHistory, got {other:?}"),
        }
    }

    /// A `/retry` refused mid-stream changes nothing: the old reply stays on
    /// screen and in the store, and the agent is not reseeded.
    #[tokio::test]
    async fn retry_refused_while_streaming_leaves_the_exchange_intact() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        app.context.append_user_message("the question").unwrap();
        app.context.append_assistant_message("old reply").unwrap();
        let sid = app.context.session_id.clone().expect("bound session");
        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: vec![],
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };

        app.handle_command("retry").await.unwrap();

        assert_eq!(app.context.messages.len(), 2, "the screen keeps the reply");
        let stored = app.context.session_store.get_messages(&sid).unwrap();
        assert_eq!(stored.len(), 2, "the store keeps the reply");
        assert!(req_rx.try_recv().is_err(), "nothing is sent to the agent");
        assert!(app
            .context
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("Cannot retry"));
    }

    /// Picking a session sends its conversational turns to the agent, and none
    /// of the session that was on screen before.
    #[tokio::test]
    async fn picking_a_session_reseeds_the_agent_with_its_turns_only() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        let session_a = store_session(
            &app.context.session_store,
            &[
                ("user", "a first question"),
                ("assistant", "a first answer"),
                ("note", "Model set to: mock-model"),
                ("user", "a second question"),
                ("assistant", "a second answer"),
            ],
        );
        let session_b = store_session(
            &app.context.session_store,
            &[("user", "b question"), ("assistant", "b answer")],
        );
        // The screen is on B, so the agent holds B's turns.
        app.context.session_id = Some(session_b);
        app.state = AppState::Ready;

        pick_session(&mut app, &session_a).await;

        match req_rx.try_recv() {
            Ok(TurnRequest::SetHistory(turns)) => {
                let shape: Vec<(&str, &str)> = turns
                    .iter()
                    .map(|(r, c)| (r.as_str(), c.as_str()))
                    .collect();
                assert_eq!(
                    shape,
                    [
                        ("user", "a first question"),
                        ("assistant", "a first answer"),
                        ("user", "a second question"),
                        ("assistant", "a second answer"),
                    ],
                    "the picked session's turns, in order, and no note"
                );
            }
            other => panic!("expected SetHistory, got {other:?}"),
        }
    }

    /// Switching sessions starts "this session" over: the total counted on
    /// the screen before the pick belonged to a different session. Resumed
    /// sessions start unreported because the store keeps no per-turn usage.
    #[tokio::test]
    async fn picking_a_session_resets_the_usage_total() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        let session_a = store_session(&app.context.session_store, &[("user", "a question")]);
        app.context.token_usage = Some(crate::tui::context::TokenUsage {
            prompt_tokens: 33,
            completion_tokens: 21,
            total_tokens: 54,
        });

        pick_session(&mut app, &session_a).await;

        assert!(
            app.context.token_usage.is_none(),
            "a picked session starts unreported, not with the previous total"
        );
    }

    /// Picking a session changes the history, not the agent, so the status bar
    /// keeps naming the model that answers the next message. A session opened
    /// under another model says so once, and says where to change it.
    #[tokio::test]
    async fn picking_a_session_from_another_model_notes_it_and_leaves_the_label_alone() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        let session = store_session_with_model(
            &app.context.session_store,
            "openrouter:other-model",
            &[("user", "a question"), ("assistant", "an answer")],
        );
        app.state = AppState::Ready;

        pick_session(&mut app, &session).await;

        assert_eq!(
            app.context.model, "mock-model",
            "the label still names the model the running agent answers with"
        );
        let notes: Vec<&str> = app
            .context
            .messages
            .iter()
            .filter(|m| m.content.contains("openrouter:other-model"))
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(notes.len(), 1, "one note, once: {notes:?}");
        assert!(
            notes[0].contains("/model"),
            "the note points at the command that changes it: {:?}",
            notes[0]
        );
    }

    /// A session that names no other model says nothing about one: neither a
    /// row that recorded none, nor one recorded against a status bar that names
    /// no model. The note the second shape would draw reads "the agent still
    /// runs ." — an empty label in the middle of a sentence.
    #[tokio::test]
    async fn picking_a_session_with_no_model_recorded_says_nothing_about_the_model() {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        let session =
            store_session_with_model(&app.context.session_store, "", &[("user", "a question")]);
        app.state = AppState::Ready;

        pick_session(&mut app, &session).await;

        assert_eq!(
            app.context.model, "mock-model",
            "the label still names the model the running agent answers with"
        );
        let about_models: Vec<&str> = app
            .context
            .messages
            .iter()
            .filter(|m| m.content.to_lowercase().contains("model"))
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            about_models.is_empty(),
            "there is no other model to name: {about_models:?}"
        );

        // The mirror: a row that DID record a model, picked while the status
        // bar names none. There is still no running model to contrast it with.
        let recorded = store_session_with_model(
            &app.context.session_store,
            "openai:other-model",
            &[("user", "another question")],
        );
        app.context.model = String::new();

        pick_session(&mut app, &recorded).await;

        assert!(
            app.context.model.is_empty(),
            "the label is left alone whatever the row recorded"
        );
        let about_models: Vec<&str> = app
            .context
            .messages
            .iter()
            .filter(|m| m.content.to_lowercase().contains("model"))
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            about_models.is_empty(),
            "an empty running label is not a model to name: {about_models:?}"
        );
    }

    // ── a message refused for its image ─────────────────────────────────
    //
    // The agent refuses such a message before it enters the history and says
    // so with a typed event. These drive a real `TuiAgentActor` over a real
    // channel, so what the TUI acts on is what a refused turn really sends.

    /// A vision-capable provider that records every request, so a test can
    /// read what the model was actually sent.
    struct VisionEcho {
        seen: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl crate::providers::Provider for VisionEcho {
        fn capabilities(&self) -> crate::providers::traits::ProviderCapabilities {
            crate::providers::traits::ProviderCapabilities {
                native_tool_calling: false,
                vision: true,
            }
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<String> {
            Ok("an answer".to_string())
        }

        async fn chat(
            &self,
            request: crate::providers::ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> anyhow::Result<crate::providers::ChatResponse> {
            self.seen.lock().unwrap().push(
                request
                    .messages
                    .iter()
                    .map(|m| m.content.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            Ok(crate::providers::ChatResponse {
                usage: None,
                text: Some("an answer".to_string()),
                tool_calls: vec![],
            })
        }
    }

    /// An app over an in-memory store whose agent is a vision agent behind a
    /// live actor: `(app, what the provider was sent, profile root)`. The
    /// root is held so nothing in the test can reach the developer's own
    /// `~/.rantaiclaw`.
    fn app_with_vision_agent() -> (TuiApp, Arc<Mutex<Vec<String>>>, tempfile::TempDir) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let agent = crate::agent::Agent::builder()
            .provider(Box::new(VisionEcho { seen: seen.clone() }))
            .tools(vec![])
            .memory(Arc::from(
                crate::memory::create_memory(
                    &crate::config::MemoryConfig {
                        backend: "none".into(),
                        ..crate::config::MemoryConfig::default()
                    },
                    std::path::Path::new("/tmp"),
                    None,
                )
                .expect("memory"),
            ))
            .observer(Arc::from(crate::observability::NoopObserver {}))
            .tool_dispatcher(Box::new(crate::agent::dispatcher::XmlToolDispatcher))
            .workspace_dir(std::path::PathBuf::from("/tmp"))
            .build()
            .expect("agent builder");
        let (req_tx, req_rx) = tokio::sync::mpsc::channel(8);
        let (events_tx, events_rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(
            crate::tui::async_bridge::TuiAgentActor::new(Some(agent), req_rx, events_tx).run(),
        );
        let store = SessionStore::in_memory().expect("in-memory store");
        let ctx = TuiContext::new(store, "mock-model", None, req_tx, events_rx).expect("context");
        let root = tempfile::TempDir::new().expect("temp profile root");
        let app = app_with_profile_root(ctx, root.path().to_path_buf());
        (app, seen, root)
    }

    /// Drain the bridge into the app until `done` holds. The TUI drains once
    /// per frame; a test drains in a loop, and says what it was waiting for.
    async fn pump_until(app: &mut TuiApp, what: &str, done: impl Fn(&TuiApp) -> bool) {
        for _ in 0..400 {
            app.drain_events();
            if done(app) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the app never reached {what}");
    }

    /// Every row the store holds for the app's session, as `(role, content)`.
    fn stored_rows(app: &TuiApp) -> Vec<(String, String)> {
        let sid = app.context.session_id.clone().expect("a bound session");
        app.context
            .session_store
            .get_messages(&sid)
            .expect("stored messages")
            .into_iter()
            .map(|m| (m.role, m.content))
            .collect()
    }

    /// The turns a reseed from the store would send to the agent.
    fn turns_in_the_store(app: &TuiApp) -> Vec<(String, String)> {
        let sid = app.context.session_id.clone().expect("a bound session");
        let stored = app
            .context
            .session_store
            .get_messages(&sid)
            .expect("stored messages");
        crate::sessions::turns_for_history(&stored)
    }

    /// The title the store holds for a session — what the picker lists it
    /// under, or `None` for `(untitled)`.
    fn title_of(app: &TuiApp, sid: &str) -> Option<String> {
        app.context
            .session_store
            .get_session(sid)
            .expect("the session opens")
            .expect("the session is stored")
            .title
    }

    /// A message whose image cannot be read: the file is not there, so the
    /// agent refuses it before the turn runs.
    const UNREADABLE_IMAGE_MESSAGE: &str = "look at this [IMAGE:/tmp/missing-picture.png]";

    #[tokio::test]
    async fn a_refused_image_is_taken_back_and_the_next_turns_are_answered() {
        let (mut app, seen, _root) = app_with_vision_agent();

        app.context.input_buffer = UNREADABLE_IMAGE_MESSAGE.to_string();
        app.submit_input().await.expect("the message is submitted");
        assert!(
            stored_rows(&app).iter().any(|(_, c)| c.contains("[IMAGE:")),
            "the row is on the screen before the turn runs"
        );

        pump_until(&mut app, "the refusal", |app| {
            matches!(app.state, AppState::Ready)
                && !app
                    .context
                    .messages
                    .iter()
                    .any(|m| m.content.contains("[IMAGE:"))
        })
        .await;

        // (a) The refused row is gone from the screen and from the store, so
        // no reseed, `/undo`, `/retry` or resume can bring it back.
        let rows = stored_rows(&app);
        assert!(
            !app.context
                .messages
                .iter()
                .any(|m| m.content.contains("[IMAGE:")),
            "the screen keeps no refused image"
        );
        assert!(
            !rows.iter().any(|(_, c)| c.contains("[IMAGE:")),
            "the store keeps no refused image: {rows:?}"
        );
        assert!(
            turns_in_the_store(&app)
                .iter()
                .all(|(_, c)| !c.contains("[IMAGE:")),
            "a reseed from the store carries no image marker"
        );

        // (b) Its text is back in the input box, ready to edit and send again.
        assert_eq!(
            app.context.input_buffer, UNREADABLE_IMAGE_MESSAGE,
            "the refused text is back in the input box"
        );
        assert_eq!(
            app.context.cursor_pos,
            UNREADABLE_IMAGE_MESSAGE.chars().count(),
            "the cursor sits at the end of the restored text"
        );

        // (c) The refusal is one line, and no "[no response …]" follows it.
        assert_eq!(
            rows.len(),
            1,
            "the refusal is the one row the turn added: {rows:?}"
        );
        assert_eq!(rows[0].0, "note", "{rows:?}");
        assert!(
            rows[0].1.starts_with("This message was not sent:"),
            "{:?}",
            rows[0].1
        );
        assert!(
            !app.context
                .messages
                .iter()
                .any(|m| m.content.starts_with("[no response from model")),
            "an empty reply line must not follow a refusal: {:?}",
            app.context.messages
        );

        // The turns after it are answered, before and after `/undo`.
        app.context.input_buffer = "what did I just ask?".to_string();
        app.submit_input()
            .await
            .expect("the plain turn is submitted");
        pump_until(&mut app, "the plain reply", |app| {
            app.context.messages.iter().any(|m| m.role == "assistant")
        })
        .await;

        app.handle_command("undo").await.expect("/undo runs");
        assert!(
            !app.context.messages.iter().any(|m| m.role == "assistant"),
            "/undo took the exchange back"
        );
        assert!(
            turns_in_the_store(&app)
                .iter()
                .all(|(_, c)| !c.contains("[IMAGE:")),
            "the reseed /undo sends holds no image marker"
        );

        app.context.input_buffer = "and again?".to_string();
        app.submit_input().await.expect("the turn after /undo");
        pump_until(&mut app, "the reply after /undo", |app| {
            matches!(app.state, AppState::Ready)
                && app.context.messages.iter().any(|m| m.role == "assistant")
        })
        .await;

        let sent = seen.lock().unwrap().join("\n");
        assert!(
            !sent.contains("[IMAGE:"),
            "no image marker ever reached the model: {sent}"
        );
        assert!(sent.contains("what did I just ask?"), "{sent}");
    }

    /// A session that still holds a refused image row — left by an older
    /// build, or written by another surface — must not break the turns after
    /// it is resumed through the picker: the row's image cannot be prepared,
    /// so its text is kept and the next turn is answered.
    #[tokio::test]
    async fn a_resumed_session_that_still_holds_an_image_row_still_answers() {
        let (mut app, seen, _root) = app_with_vision_agent();
        let session = store_session(
            &app.context.session_store,
            &[
                ("user", UNREADABLE_IMAGE_MESSAGE),
                ("assistant", "I could not read that picture"),
            ],
        );
        app.state = AppState::Ready;

        pick_session(&mut app, &session).await;

        app.context.input_buffer = "what did I ask?".to_string();
        app.submit_input().await.expect("the turn after the resume");
        pump_until(&mut app, "the reply after the resume", |app| {
            app.context
                .messages
                .iter()
                .any(|m| m.role == "assistant" && m.content == "an answer")
        })
        .await;

        let sent = seen.lock().unwrap().join("\n");
        assert!(
            !sent.contains("[IMAGE:"),
            "the stale marker never reached the model: {sent}"
        );
        assert!(
            sent.contains("look at this"),
            "the stored turn kept its text: {sent}"
        );
    }

    /// `/new` opens a session with a "Started new session" note, so the title
    /// cannot come from "the first row": it comes from the first user row, and
    /// the picker lists the session under it.
    #[tokio::test]
    async fn a_session_started_with_new_takes_the_title_of_its_first_user_message() {
        let (mut app, _seen, _root) = app_with_vision_agent();

        app.handle_command("new").await.expect("/new runs");
        let sid = app
            .context
            .session_id
            .clone()
            .expect("the note binds a session");
        assert!(
            title_of(&app, &sid).is_none(),
            "a session that holds only a note is untitled"
        );

        app.context.input_buffer = "design a cache eviction policy".to_string();
        app.submit_input()
            .await
            .expect("the first message is submitted");

        assert_eq!(
            title_of(&app, &sid).as_deref(),
            Some("design a cache eviction policy"),
            "the session is listed under its first user message"
        );
    }

    /// A refused first message titles nothing — the row it would have named is
    /// gone — and the next user message names the session. The refused turn
    /// adds exactly one line.
    #[tokio::test]
    async fn a_refused_first_message_leaves_the_session_untitled_and_the_next_one_names_it() {
        let (mut app, _seen, _root) = app_with_vision_agent();
        app.handle_command("new").await.expect("/new runs");
        let sid = app
            .context
            .session_id
            .clone()
            .expect("the note binds a session");
        let before = stored_rows(&app).len();

        app.context.input_buffer = UNREADABLE_IMAGE_MESSAGE.to_string();
        app.submit_input()
            .await
            .expect("the refused message is submitted");
        pump_until(&mut app, "the refusal", |app| {
            matches!(app.state, AppState::Ready)
                && !app
                    .context
                    .messages
                    .iter()
                    .any(|m| m.content.contains("[IMAGE:"))
        })
        .await;

        let rows = stored_rows(&app);
        assert_eq!(
            rows.len(),
            before + 1,
            "the refusal is the only line the refused turn added: {rows:?}"
        );
        assert_eq!(rows[before].0, "note", "{rows:?}");
        assert!(
            title_of(&app, &sid).is_none(),
            "a session whose only user row was refused is untitled"
        );

        app.context.input_buffer = "design a cache eviction policy".to_string();
        app.submit_input()
            .await
            .expect("the next message is submitted");

        assert_eq!(
            title_of(&app, &sid).as_deref(),
            Some("design a cache eviction policy"),
            "the next user message names the session"
        );
    }

    /// A title set by hand with `/title` is the operator's: a refused message
    /// does not clear it, and no later message derives over it. `/new` starts
    /// a different session, which titles itself again.
    #[tokio::test]
    async fn a_title_set_by_hand_survives_a_refusal_and_does_not_outlive_new() {
        let (mut app, _seen, _root) = app_with_vision_agent();
        app.handle_command("new").await.expect("/new runs");
        let titled = app
            .context
            .session_id
            .clone()
            .expect("the note binds a session");
        app.handle_command("title my-session")
            .await
            .expect("/title runs");

        // The refused message is the session's first user row — the row a
        // derived title would have come from.
        app.context.input_buffer = UNREADABLE_IMAGE_MESSAGE.to_string();
        app.submit_input()
            .await
            .expect("the refused message is submitted");
        pump_until(&mut app, "the refusal", |app| {
            matches!(app.state, AppState::Ready)
                && !app
                    .context
                    .messages
                    .iter()
                    .any(|m| m.content.contains("[IMAGE:"))
        })
        .await;

        assert_eq!(
            title_of(&app, &titled).as_deref(),
            Some("my-session"),
            "a hand-set title is kept"
        );

        app.context.input_buffer = "a plain question".to_string();
        app.submit_input()
            .await
            .expect("the next message is submitted");
        assert_eq!(
            title_of(&app, &titled).as_deref(),
            Some("my-session"),
            "no later message derives over a hand-set title"
        );

        app.handle_command("new").await.expect("/new runs");
        let fresh = app
            .context
            .session_id
            .clone()
            .expect("the note binds a session");
        app.context.input_buffer = "a fresh question".to_string();
        app.submit_input()
            .await
            .expect("the first message is submitted");
        assert_eq!(
            title_of(&app, &fresh).as_deref(),
            Some("a fresh question"),
            "the new session titles itself; the old session's title does not follow"
        );
    }

    /// A message submitted while another turn is still in flight waits its
    /// turn, so a refusal can arrive when the screen holds more than one user
    /// row. The refusal takes back ITS OWN row: the refused message goes from
    /// the screen and from the store and its text returns to the input box,
    /// while the message queued behind it keeps its row and its answer.
    #[tokio::test]
    async fn a_refused_message_is_taken_back_while_another_one_waits_behind_it() {
        let (mut app, seen, _root) = app_with_vision_agent();

        app.context.input_buffer = UNREADABLE_IMAGE_MESSAGE.to_string();
        app.submit_input()
            .await
            .expect("the refused message is submitted");
        // Submitted while the refused turn is still in flight, so BOTH rows
        // are on screen before the refusal is handled.
        app.context.input_buffer = "what did I just ask?".to_string();
        app.submit_input()
            .await
            .expect("the second message is submitted");
        assert_eq!(app.context.queued_turns, 1, "the second turn waits");

        pump_until(&mut app, "the reply to the queued message", |app| {
            matches!(app.state, AppState::Ready)
                && app.context.messages.iter().any(|m| m.role == "assistant")
        })
        .await;

        let rows = stored_rows(&app);
        // (a) The refused message is the one that went — from the screen and
        // from the store — so no reseed, `/undo` or resume brings it back.
        assert!(
            !app.context
                .messages
                .iter()
                .any(|m| m.content.contains("[IMAGE:")),
            "the screen keeps no refused image"
        );
        assert!(
            !rows.iter().any(|(_, c)| c.contains("[IMAGE:")),
            "the store keeps no refused image: {rows:?}"
        );
        // (b) The message queued behind it is untouched: same row, same text.
        assert_eq!(
            rows.iter()
                .filter(|(role, _)| role == "user")
                .map(|(_, content)| content.as_str())
                .collect::<Vec<_>>(),
            ["what did I just ask?"],
            "the queued message keeps its own row: {rows:?}"
        );
        // (c) The refused text is back in the input box, ready to edit.
        assert_eq!(
            app.context.input_buffer, UNREADABLE_IMAGE_MESSAGE,
            "the refused text is back in the input box"
        );
        // (d) The queued message is answered, with no empty reply line and no
        // empty assistant row left over from the turn that never ran.
        assert!(
            app.context
                .messages
                .iter()
                .any(|m| m.role == "assistant" && m.content == "an answer"),
            "the queued message is answered: {:?}",
            rows
        );
        assert!(
            !app.context
                .messages
                .iter()
                .any(|m| m.content.starts_with("[no response from model")),
            "no empty reply line follows the refusal"
        );

        let sent = seen.lock().unwrap().join("\n");
        assert!(
            !sent.contains("[IMAGE:"),
            "no image marker ever reached the model: {sent}"
        );
        assert!(sent.contains("what did I just ask?"), "{sent}");
    }

    /// A `/retry` submits a turn for a row that is already on the screen, so
    /// a refusal of it takes back THAT row — the retried one — even though the
    /// resubmit appended nothing.
    #[tokio::test]
    async fn a_refused_retry_takes_back_the_retried_row() {
        let (mut app, seen, _root) = app_with_vision_agent();
        let session = store_session(
            &app.context.session_store,
            &[
                ("user", UNREADABLE_IMAGE_MESSAGE),
                ("assistant", "I could not read that picture"),
            ],
        );
        app.state = AppState::Ready;
        pick_session(&mut app, &session).await;

        app.handle_command("retry").await.expect("/retry runs");
        pump_until(&mut app, "the refusal", |app| {
            matches!(app.state, AppState::Ready)
                && !app
                    .context
                    .messages
                    .iter()
                    .any(|m| m.content.contains("[IMAGE:"))
        })
        .await;

        let rows = stored_rows(&app);
        assert!(
            !rows.iter().any(|(_, c)| c.contains("[IMAGE:")),
            "the store keeps no refused image: {rows:?}"
        );
        assert_eq!(
            app.context.input_buffer, UNREADABLE_IMAGE_MESSAGE,
            "the retried text is back in the input box"
        );
        assert!(
            !seen.lock().unwrap().join("\n").contains("[IMAGE:"),
            "no image marker ever reached the model"
        );
    }

    /// Take one event off the bridge and hand it to the app, as a frame does.
    /// `drain_events` takes everything the channel holds in a single pass, so
    /// this is how a test places an action between two events the agent sent
    /// back to back.
    async fn next_event(app: &mut TuiApp) -> AgentEvent {
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            app.context.events_rx.recv(),
        )
        .await
        .expect("the agent sends an event")
        .expect("the bridge stays open")
    }

    /// A refused turn never ran, so the `Done` that ends it must add no
    /// reply — not even when the operator submits the next message in the
    /// window between the refusal and that `Done`. A turn-end marker the
    /// submit clears is a turn-end marker the wrong turn can swallow.
    #[tokio::test]
    async fn a_done_after_a_refusal_adds_no_reply_even_when_a_message_was_submitted_in_between() {
        let (mut app, _seen, _root) = app_with_vision_agent();

        app.context.input_buffer = UNREADABLE_IMAGE_MESSAGE.to_string();
        app.submit_input()
            .await
            .expect("the refused message is submitted");

        let refusal = next_event(&mut app).await;
        assert!(
            matches!(refusal, AgentEvent::ImageRefused { .. }),
            "the turn was refused, not failed: {refusal:?}"
        );
        app.handle_agent_event(refusal);

        // Inside the window: the next message is submitted before the `Done`
        // that ends the refused turn has been handled.
        app.context.input_buffer = "what did I just ask?".to_string();
        app.submit_input()
            .await
            .expect("the next message is submitted inside the window");

        let done = next_event(&mut app).await;
        assert!(
            matches!(done, AgentEvent::Done { .. }),
            "the refused turn ends with its own Done: {done:?}"
        );
        app.handle_agent_event(done);

        assert!(
            !app.context
                .messages
                .iter()
                .any(|m| m.content.starts_with("[no response from model")),
            "the Done that ends a refused turn prints no empty-reply line: {:?}",
            stored_rows(&app)
        );
        assert_eq!(
            app.context
                .messages
                .iter()
                .filter(|m| m.role == "assistant")
                .count(),
            0,
            "and commits no empty assistant row: {:?}",
            stored_rows(&app)
        );
        assert_eq!(
            app.context.queued_turns, 0,
            "the turn that never ran is over; the queued one is now in flight"
        );

        // The queued message is answered as usual.
        pump_until(&mut app, "the reply to the queued message", |app| {
            app.context.messages.iter().any(|m| m.role == "assistant")
        })
        .await;
    }

    /// A turn the actor could not run ends when its error is surfaced. With no
    /// agent there was no turn to run, so the actor answers the `Submit` with
    /// the no-agent error and sends no `Done` — the TUI retires the queued turn
    /// itself. Left in the queue, that dead entry sits at the front: the first
    /// refusal after the session heals takes back ITS row, deleting a message
    /// typed before the reload from the screen and the store and putting THAT
    /// text in the input box, while the message that was actually refused
    /// survives — the defect this whole path exists to stop.
    ///
    /// The events are placed by hand because staging "no agent, then a reload
    /// that heals it" against a live actor needs a config that builds a
    /// vision-capable agent: a provider without vision refuses an unreadable
    /// image with a plain `Error` (the loop's capability gate), never
    /// `ImageRefused`, and no provider factory that builds offline is
    /// vision-capable. The TUI acts on the event stream alone, so the test
    /// hands it the stream that sequence really produces.
    #[tokio::test]
    async fn a_refusal_after_a_turn_the_actor_never_ran_takes_back_its_own_row() {
        let (ctx, mut req_rx, _events_tx) = TuiContext::test_context();
        let mut app = make_app_with_context(ctx);
        // What `run_tui` records when the boot-time `Agent::from_config`
        // failed: the actor runs with no agent until a reload heals it.
        app.actor_has_agent = false;

        // (1) A message the actor cannot run. The row is written and the turn
        // queued, exactly as it is for any other message.
        app.context.input_buffer = "hello".to_string();
        app.submit_input().await.expect("the message is submitted");
        assert!(
            matches!(req_rx.try_recv(), Ok(TurnRequest::Submit { .. })),
            "the turn reached the actor"
        );
        assert_eq!(
            app.pending_turns.len(),
            1,
            "the turn is queued while it is unanswered"
        );
        // The only event back. The TUI does not read this text — an error it
        // cannot pair with a `Done` is a fact about the actor, not the words.
        app.handle_agent_event(AgentEvent::Error(
            "No working provider — fix it via /setup provider.".to_string(),
        ));
        assert!(
            app.pending_turns.is_empty(),
            "an error the actor sends no Done for must not leave its turn queued: {:?}",
            app.pending_turns
        );
        assert!(
            app.context
                .messages
                .iter()
                .any(|m| m.role == "user" && m.content == "hello"),
            "the message stays on the screen, as it does today"
        );

        // (2) `/model` heals the session: the actor holds an agent again.
        app.handle_agent_event(AgentEvent::ReloadComplete {
            id: 0,
            mcp_servers_configured: vec![],
            mcp_tools_by_server: std::collections::HashMap::new(),
            security: None,
        });

        // (3) A message whose image cannot be read is refused, and takes back
        // its own row.
        app.context.input_buffer = UNREADABLE_IMAGE_MESSAGE.to_string();
        app.submit_input()
            .await
            .expect("the refused message is submitted");
        app.handle_agent_event(AgentEvent::ImageRefused {
            message: "This message was not sent: the picture could not be read.".to_string(),
        });
        app.handle_agent_event(AgentEvent::Done {
            final_text: String::new(),
            cancelled: false,
        });

        let rows = stored_rows(&app);
        assert!(
            !app.context
                .messages
                .iter()
                .any(|m| m.content.contains("[IMAGE:")),
            "the refused row leaves the screen: {:?}",
            app.context.messages
        );
        assert!(
            !rows.iter().any(|(_, c)| c.contains("[IMAGE:")),
            "the refused row leaves the store: {rows:?}"
        );
        assert!(
            rows.iter().any(|(role, c)| role == "user" && c == "hello"),
            "the message from before the reload survives: {rows:?}"
        );
        assert!(
            app.context
                .messages
                .iter()
                .any(|m| m.role == "user" && m.content == "hello"),
            "the message from before the reload is still on the screen"
        );
        assert_eq!(
            app.context.input_buffer, UNREADABLE_IMAGE_MESSAGE,
            "the refused text is the text that comes back"
        );
        assert!(
            app.pending_turns.is_empty(),
            "the refusal and its Done closed one turn each: {:?}",
            app.pending_turns
        );

        // (4) The next turn is answered by its own `Done` — the queue is not
        // off by one, which is how a dead entry would show up here.
        app.context.input_buffer = "are you there?".to_string();
        app.submit_input()
            .await
            .expect("the turn after the refusal is submitted");
        assert_eq!(app.pending_turns.len(), 1, "one queued turn");
        app.handle_agent_event(AgentEvent::Done {
            final_text: "an answer".to_string(),
            cancelled: false,
        });
        assert!(
            app.context
                .messages
                .iter()
                .any(|m| m.role == "assistant" && m.content == "an answer"),
            "the turn after the refusal is answered: {:?}",
            stored_rows(&app)
        );
        assert!(app.pending_turns.is_empty(), "{:?}", app.pending_turns);
    }

    /// `/title` belongs to the session it was set on. The picker binds another
    /// one, whose first user row has to be able to name it — otherwise the
    /// picked session stays `(untitled)` for the rest of its life.
    #[tokio::test]
    async fn picking_a_session_lets_its_own_first_user_message_name_it() {
        let (mut app, _seen, _root) = app_with_vision_agent();

        app.handle_command("new").await.expect("/new runs");
        let named = app
            .context
            .session_id
            .clone()
            .expect("the note binds a session");
        app.handle_command("title my-session")
            .await
            .expect("/title runs");
        assert!(
            app.context.title_set_by_hand,
            "the session on screen was named by hand"
        );

        // Another session, with no title of its own.
        let picked = store_session(
            &app.context.session_store,
            &[("note", "Started new session")],
        );
        app.state = AppState::Ready;
        pick_session(&mut app, &picked).await;

        app.context.input_buffer = "design a cache eviction policy".to_string();
        app.submit_input()
            .await
            .expect("the first message of the picked session is submitted");

        assert_eq!(
            title_of(&app, &picked).as_deref(),
            Some("design a cache eviction policy"),
            "the picked session is listed under its first user message"
        );
        assert_eq!(
            title_of(&app, &named).as_deref(),
            Some("my-session"),
            "the session that was named by hand keeps its title"
        );
    }
}

#[cfg(test)]
mod autonomy_keybinding_tests {
    use super::test_support::app_with_profile_root;
    use super::{AppState, EventResult};
    use crate::approval::policy_writer::{self, PolicyPreset};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// Shift+Tab as the terminal actually delivers it: `BackTab` already
    /// carries SHIFT, which is what let this arm fire from inside an approval
    /// prompt in the first place.
    fn shift_tab() -> KeyEvent {
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
    }

    fn preset_on_disk(root: &std::path::Path) -> Option<PolicyPreset> {
        policy_writer::read_active_preset(&root.join("policy"))
    }

    /// A blocked press must leave the profile untouched — no `autonomy.toml`
    /// at all, since nothing has written one yet.
    async fn press_shift_tab(app: &mut super::TuiApp) {
        let result = app.handle_key(shift_tab()).await.expect("handle_key");
        assert!(matches!(result, EventResult::Continue));
    }

    /// The control. Without it, every assertion below passes against a
    /// keybinding that never worked at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shift_tab_cycles_the_preset_when_nothing_is_on_screen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = app_with_profile_root(ctx, dir.path().to_path_buf());
        // `app_with_profile_root` already points `config_path` at the tempdir;
        // `workspace_dir` still needs pinning explicitly since the
        // constructor leaves it at `Config::default()`'s real-`$HOME` value.
        app.config.workspace_dir = dir.path().to_path_buf();

        assert_eq!(preset_on_disk(dir.path()), None, "nothing written yet");
        press_shift_tab(&mut app).await;

        // No `autonomy.toml` reads as `Smart`, so one step lands on `Strict`.
        assert_eq!(
            preset_on_disk(dir.path()),
            Some(PolicyPreset::Strict),
            "the keybinding must actually cycle when unguarded"
        );
    }

    /// The defect plan 136 fixed, and the one that had no test: the operator
    /// is looking at an approval gate while the keypress removes gating.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shift_tab_cannot_change_autonomy_from_inside_an_approval_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = app_with_profile_root(ctx, dir.path().to_path_buf());

        app.pending_approval = Some(crate::security::PendingRequest {
            id: uuid::Uuid::new_v4(),
            basename: "curl".to_string(),
            full_command: "curl https://example.com".to_string(),
            channel: "tui".to_string(),
            reply_target: String::new(),
            created_at: 0,
            always_available: false,
        });

        press_shift_tab(&mut app).await;

        assert_eq!(
            preset_on_disk(dir.path()),
            None,
            "autonomy changed while an approval was on screen"
        );
    }

    /// The setup wizard owns the screen. Cycling autonomy underneath it is a
    /// state change the operator did not ask for and cannot see.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shift_tab_cannot_change_autonomy_from_inside_the_setup_overlay() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = app_with_profile_root(ctx, dir.path().to_path_buf());

        app.setup_overlay = Some(crate::tui::SetupOverlayState::new("Setup"));

        press_shift_tab(&mut app).await;

        assert_eq!(
            preset_on_disk(dir.path()),
            None,
            "autonomy changed while the setup overlay owned the screen"
        );
    }

    /// Same for the first-run wizard, which owns the screen before the operator
    /// has an approval policy to speak of.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shift_tab_cannot_change_autonomy_from_inside_the_first_run_wizard() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = app_with_profile_root(ctx, dir.path().to_path_buf());

        app.first_run_wizard = Some(crate::tui::FirstRunWizard::new(crate::profile::Profile {
            name: "default".to_string(),
            root: dir.path().to_path_buf(),
        }));

        press_shift_tab(&mut app).await;

        assert_eq!(
            preset_on_disk(dir.path()),
            None,
            "autonomy changed while the first-run wizard owned the screen"
        );
    }

    /// A turn in flight owns the screen; the policy must not move under it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shift_tab_cannot_change_autonomy_while_a_turn_is_streaming() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = app_with_profile_root(ctx, dir.path().to_path_buf());

        app.state = AppState::Streaming {
            partial: String::new(),
            tool_blocks: Vec::new(),
            cancelling: false,
            turn_started_at: std::time::Instant::now(),
        };

        press_shift_tab(&mut app).await;

        assert_eq!(
            preset_on_disk(dir.path()),
            None,
            "autonomy changed while a turn was streaming"
        );
    }

    /// The contract the status bar advertises: "Shift+Tab cycles
    /// Manual→Smart→Strict". Two presses must therefore land on two different
    /// rungs — and the cycle must never land on `Off`, which disables the
    /// approval gate and is reachable only through `/autonomy off`.
    ///
    /// This is what caught the marker bug: `write_policy_files(.., force=false)`
    /// leaves an existing `autonomy.toml` alone, so on a configured profile the
    /// preset never moved and the second press re-announced the first rung.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shift_tab_advances_the_preset_on_every_press_and_skips_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = crate::profile::Profile {
            name: "default".to_string(),
            root: dir.path().to_path_buf(),
        };
        // Seed a configured profile — the case that used to be stuck. A fresh
        // profile with no `autonomy.toml` was the only one that ever cycled.
        policy_writer::write_policy_files(&profile, PolicyPreset::Manual, false)
            .expect("seed the profile at Manual");
        assert_eq!(preset_on_disk(dir.path()), Some(PolicyPreset::Manual));

        let (ctx, _req_rx, _events_tx) = crate::tui::context::TuiContext::test_context();
        let mut app = app_with_profile_root(ctx, dir.path().to_path_buf());
        // `app_with_profile_root` already points `config_path` at the tempdir;
        // `workspace_dir` still needs pinning explicitly since the
        // constructor leaves it at `Config::default()`'s real-`$HOME` value.
        app.config.workspace_dir = dir.path().to_path_buf();

        press_shift_tab(&mut app).await;
        assert_eq!(
            preset_on_disk(dir.path()),
            Some(PolicyPreset::Smart),
            "first press must advance Manual -> Smart"
        );

        press_shift_tab(&mut app).await;
        assert_eq!(
            preset_on_disk(dir.path()),
            Some(PolicyPreset::Strict),
            "second press must advance again — a cycle that repeats one rung is stuck"
        );

        // Strict is the rung before `Off` in the canonical order. The keybinding
        // wraps to Manual instead: removing the gate is an explicit act.
        press_shift_tab(&mut app).await;
        assert_eq!(
            preset_on_disk(dir.path()),
            Some(PolicyPreset::Manual),
            "the keybinding cycle must skip Off — it disables the approval gate"
        );
    }
}

#[cfg(test)]
mod scrollback_cast_tests {
    /// Mirror the saturating `usize -> u16` conversion used in
    /// `commit_lines_to_scrollback`. A `usize::MAX` row count must clamp to
    /// `u16::MAX` rather than wrap, otherwise the subsequent
    /// `row_count.min(max_chunk)` comparison would lose information for
    /// extreme-but-valid inputs.
    #[test]
    fn usize_max_row_count_clamps_to_u16_max() {
        let row_count = usize::MAX;
        let clamped = u16::try_from(row_count).unwrap_or(u16::MAX);
        assert_eq!(clamped, u16::MAX);
    }
}

#[cfg(test)]
mod resume_view_tests {
    use super::restore_resumed_session;
    use crate::agent::door_test_support::DoorFixture;
    use crate::agent::Agent;

    /// Resuming a session rebuilds the system prompt from the stored turns,
    /// before the actor starts the first turn. The TUI reads all of memory, so
    /// that prompt carries the owner's files.
    #[tokio::test]
    async fn resuming_a_session_sends_the_owner_files() {
        let fixture = DoorFixture::start().await;
        let session_id = crate::sessions::cli::open_store()
            .expect("the sessions store opens")
            .record_api_turn(
                "mock-model",
                None,
                "an earlier question",
                "an earlier answer",
            )
            .expect("a session is stored");

        let mut agent = Agent::from_config(&fixture.config)
            .await
            .expect("the agent builds");
        restore_resumed_session(&mut agent, &session_id).await;
        crate::memory::MEMORY_VIEW
            .scope(
                crate::memory::MemoryView::All,
                agent.turn("what about the lantern"),
            )
            .await
            .expect("the turn runs against the local server");

        assert_eq!(fixture.llm.request_count(), 1);
        fixture.assert_last_system_prompt_carries_the_owner_files();
    }
}
