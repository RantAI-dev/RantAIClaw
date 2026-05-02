use super::{CommandHandler, CommandResult};
use crate::tui::context::TuiContext;
use anyhow::Result;

pub struct SetupCommand;

impl CommandHandler for SetupCommand {
    fn name(&self) -> &str {
        "setup"
    }

    fn description(&self) -> &str {
        "Configure providers, channels, and integrations"
    }

    fn usage(&self) -> &str {
        "setup [provisioner-name]"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let provisioner = args.trim();
        Ok(CommandResult::OpenSetupOverlay {
            provisioner: if provisioner.is_empty() {
                None
            } else {
                Some(provisioner.to_string())
            },
        })
    }
}
