//! `/kb` — Knowledge Base status + activation toggle.
//!
//! Parity with the console's activation screen (plan 107): status shows
//! whether the KB is on and whether a key resolves; `enable`/`disable`
//! persist `[knowledge].enabled` the same way `/autonomy` persists its
//! preset (fresh load → mutate → save, picked up by the config watcher).
//! A full KB browser (list/search/graph) is deliberately out of scope.

use anyhow::Result;

use super::{CommandHandler, CommandResult, TuiContext};

pub struct KbCommand;

/// Load config.toml fresh, apply the toggle, save back. Same
/// `block_in_place` idiom as `/autonomy` — `CommandHandler::execute` is
/// sync, `Config::save` is async.
fn persist_enabled(enabled: bool) -> anyhow::Result<()> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| anyhow::anyhow!("/kb must run inside a tokio runtime"))?;
    tokio::task::block_in_place(|| {
        handle.block_on(async move {
            let mut config = crate::config::Config::load_or_init().await?;
            if enabled {
                let key_resolves = config
                    .knowledge
                    .embedding_api_key
                    .as_deref()
                    .map(|v| !v.is_empty())
                    .unwrap_or(false)
                    || std::env::var("KB_EMBEDDING_API_KEY")
                        .map(|v| !v.is_empty())
                        .unwrap_or(false)
                    || std::env::var("OPENROUTER_API_KEY")
                        .map(|v| !v.is_empty())
                        .unwrap_or(false);
                if !key_resolves {
                    anyhow::bail!("no embedding key resolves — run /setup knowledge first");
                }
            }
            config.knowledge.enabled = enabled;
            config.save().await
        })
    })
}

impl CommandHandler for KbCommand {
    fn name(&self) -> &str {
        "kb"
    }

    fn description(&self) -> &str {
        "Show Knowledge Base status, or activate/deactivate it"
    }

    fn usage(&self) -> &str {
        "/kb [enable|disable]"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        match args.trim() {
            "enable" => match persist_enabled(true) {
                Ok(()) => Ok(CommandResult::Message("✓ Knowledge Base activated".into())),
                Err(e) => Ok(CommandResult::Message(format!(
                    "✗ Could not activate the Knowledge Base: {e}"
                ))),
            },
            "disable" => match persist_enabled(false) {
                Ok(()) => Ok(CommandResult::Message(
                    "✓ Knowledge Base deactivated — credentials kept; /kb enable turns it back on"
                        .into(),
                )),
                Err(e) => Ok(CommandResult::Message(format!(
                    "✗ Could not deactivate the Knowledge Base: {e}"
                ))),
            },
            "" => {
                // Status reads the on-disk config (the TUI context does not
                // carry `knowledge`), and never opens the store — a
                // read-only existence check is enough here.
                let handle = tokio::runtime::Handle::try_current()
                    .map_err(|_| anyhow::anyhow!("/kb must run inside a tokio runtime"))?;
                let config = tokio::task::block_in_place(|| {
                    handle.block_on(crate::config::Config::load_or_init())
                })?;
                let enabled = config.knowledge.enabled;
                let key_source = if std::env::var("KB_EMBEDDING_API_KEY")
                    .map(|v| !v.is_empty())
                    .unwrap_or(false)
                {
                    "env"
                } else if config
                    .knowledge
                    .embedding_api_key
                    .as_deref()
                    .map(|v| !v.is_empty())
                    .unwrap_or(false)
                {
                    "config"
                } else if std::env::var("OPENROUTER_API_KEY")
                    .map(|v| !v.is_empty())
                    .unwrap_or(false)
                {
                    "openrouter env"
                } else {
                    "none"
                };
                let db_path = crate::kb::axi::cli::resolve_kb_db_path();
                let db_state = if db_path.exists() {
                    "present"
                } else {
                    "not created yet"
                };
                Ok(CommandResult::Message(format!(
                    "Knowledge Base: {}\n  key: {}\n  database: {} ({})\n  configure: /setup knowledge · toggle: /kb enable | /kb disable",
                    if enabled { "ACTIVE" } else { "off" },
                    key_source,
                    db_path.display(),
                    db_state,
                )))
            }
            other => Ok(CommandResult::Message(format!(
                "Unknown argument `{other}` — usage: /kb [enable|disable]"
            ))),
        }
    }
}
