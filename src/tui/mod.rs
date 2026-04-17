#[cfg(feature = "tui")]
mod app;
#[cfg(feature = "tui")]
mod commands;
#[cfg(feature = "tui")]
mod context;
#[cfg(feature = "tui")]
mod widgets;

#[cfg(feature = "tui")]
pub use app::run_tui;
#[cfg(feature = "tui")]
pub use commands::{CommandHandler, CommandRegistry, CommandResult};

use std::path::PathBuf;

/// Configuration for the TUI
#[derive(Debug, Clone)]
pub struct TuiConfig {
    pub data_dir: PathBuf,
    pub model: String,
    pub resume_session: Option<String>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        let data_dir = directories::ProjectDirs::from("", "", "rantaiclaw")
            .map(|d| d.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".rantaiclaw"));

        Self {
            data_dir,
            model: "anthropic:claude-sonnet-4-20250514".to_string(),
            resume_session: None,
        }
    }
}
