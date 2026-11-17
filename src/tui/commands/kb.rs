//! `/kb` — Knowledge Base status + activation toggle.
//!
//! Parity with the console's activation screen (plan 107): status shows
//! whether the KB is on and whether a key resolves; `enable`/`disable`
//! persist `[knowledge].enabled` the same way `/autonomy` persists its
//! preset (fresh load → mutate → save, picked up by the config watcher).
//! A full KB browser (list/search/graph) is deliberately out of scope.

use anyhow::Result;

use super::{CommandHandler, CommandResult, TuiContext};
use crate::kb::axi::activation::{embedding_key_source, set_enabled, Activation};

pub struct KbCommand;

/// Toggle `[knowledge].enabled` through the shared activation logic: it
/// probes the endpoint before enabling and returns the value it read back
/// from `config.toml`. Same `block_in_place` idiom as `/autonomy` —
/// `CommandHandler::execute` is sync, the config I/O is async.
fn persist_enabled(enabled: bool) -> anyhow::Result<Activation> {
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| anyhow::anyhow!("/kb must run inside a tokio runtime"))?;
    tokio::task::block_in_place(|| handle.block_on(set_enabled(enabled)))
        .map_err(anyhow::Error::from)
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
                Ok(Activation::Persisted(true)) => {
                    Ok(CommandResult::Message("✓ Knowledge Base activated".into()))
                }
                Ok(Activation::Persisted(false)) => Ok(CommandResult::Message(
                    "✗ Could not activate the Knowledge Base: config.toml still has it off".into(),
                )),
                Ok(Activation::Refused { message, .. }) => Ok(CommandResult::Message(format!(
                    "✗ Could not activate the Knowledge Base: {message}"
                ))),
                Err(e) => Ok(CommandResult::Message(format!(
                    "✗ Could not activate the Knowledge Base: {e}"
                ))),
            },
            "disable" => match persist_enabled(false) {
                Ok(Activation::Persisted(false)) => Ok(CommandResult::Message(
                    "✓ Knowledge Base deactivated — credentials kept; /kb enable turns it back on"
                        .into(),
                )),
                Ok(Activation::Persisted(true)) => Ok(CommandResult::Message(
                    "✗ Could not deactivate the Knowledge Base: config.toml still has it on".into(),
                )),
                Ok(Activation::Refused { message, .. }) => Ok(CommandResult::Message(format!(
                    "✗ Could not deactivate the Knowledge Base: {message}"
                ))),
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
                let key_source =
                    embedding_key_source(config.knowledge.embedding_api_key.as_deref());
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
