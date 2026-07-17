#[cfg(feature = "tui")]
mod app;
#[cfg(feature = "tui")]
pub mod async_bridge;
#[cfg(feature = "tui")]
mod commands;
#[cfg(feature = "tui")]
mod context;
#[cfg(feature = "tui")]
pub mod first_run_wizard;
#[cfg(feature = "tui")]
mod render;
#[cfg(feature = "tui")]
mod widgets;

#[cfg(feature = "tui")]
pub use app::run_tui;
#[cfg(feature = "tui")]
#[allow(unused_imports)]
pub use async_bridge::{TuiAgentActor, TurnRequest};
#[cfg(feature = "tui")]
pub use commands::{CommandHandler, CommandRegistry, CommandResult};
#[cfg(feature = "tui")]
#[allow(unused_imports)]
pub use first_run_wizard::FirstRunWizard;
#[cfg(feature = "tui")]
#[allow(unused_imports)]
pub use widgets::LoginGateState;
#[cfg(feature = "tui")]
#[allow(unused_imports)]
pub use widgets::SetupOverlayState;

/// Configuration for the TUI
#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub model: String,
    pub resume_session: Option<String>,
    /// Open a provisioner overlay on startup.
    /// `None` = no overlay (bare chat).
    /// `Some("")` = open the SetupTopic category picker.
    /// `Some(name)` = open that specific provisioner's overlay.
    pub setup_provisioner: Option<String>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            model: "anthropic:claude-sonnet-4-20250514".to_string(),
            resume_session: None,
            setup_provisioner: None,
        }
    }
}
