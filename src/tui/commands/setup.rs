use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::onboard::provision::{available, provisioner_for, ProvisionerCategory};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{ListPicker, ListPickerEntry, ListPickerItem, ListPickerKind};

pub struct SetupCommand;

impl CommandHandler for SetupCommand {
    fn name(&self) -> &str {
        "setup"
    }

    fn description(&self) -> &str {
        "Configure providers, channels, and integrations"
    }

    fn usage(&self) -> &str {
        "setup [topic]"
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let arg = args.trim();
        if !arg.is_empty() {
            return Ok(CommandResult::OpenSetupOverlay {
                provisioner: Some(arg.to_string()),
            });
        }

        let all = available();
        let mut entries: Vec<ListPickerEntry> = Vec::with_capacity(all.len());

        let mut categories: Vec<(ProvisionerCategory, &'static str)> = Vec::new();
        for &(name, _) in &all {
            let cat = provisioner_for(name)
                .map(|p| p.category())
                .unwrap_or(ProvisionerCategory::Core);
            if !categories.iter().any(|(c, _)| *c == cat) {
                categories.push((cat, cat_label(cat)));
            }
        }
        categories.sort_by_key(|(c, _)| cat_order(*c));

        for (cat, label) in categories {
            let cat_items: Vec<_> = all
                .iter()
                .filter(|(name, desc)| {
                    let c = provisioner_for(name)
                        .map(|p| p.category())
                        .unwrap_or(ProvisionerCategory::Core);
                    c == cat
                })
                .collect();

            entries.push(ListPickerEntry::category_header(
                cat_label(cat).to_lowercase(),
                label,
                cat_items.len(),
            ));

            for (name, desc) in cat_items {
                entries.push(ListPickerEntry::Item(ListPickerItem {
                    key: name.to_string(),
                    primary: name.to_string(),
                    secondary: desc.to_string(),
                }));
            }
        }

        let picker = ListPicker::with_entries(
            ListPickerKind::SetupTopic,
            "Select setup topic",
            entries,
            None,
            "no setup topics available",
        );
        Ok(CommandResult::OpenListPicker(picker))
    }
}

fn cat_label(c: ProvisionerCategory) -> &'static str {
    match c {
        ProvisionerCategory::Core => "Core",
        ProvisionerCategory::Channel => "Channels",
        ProvisionerCategory::Integration => "Integrations",
        ProvisionerCategory::Runtime => "Runtime",
        ProvisionerCategory::Hardware => "Hardware",
        ProvisionerCategory::Routing => "Routing",
    }
}

fn cat_order(c: ProvisionerCategory) -> u8 {
    match c {
        ProvisionerCategory::Core => 0,
        ProvisionerCategory::Channel => 1,
        ProvisionerCategory::Integration => 2,
        ProvisionerCategory::Runtime => 3,
        ProvisionerCategory::Hardware => 4,
        ProvisionerCategory::Routing => 5,
    }
}
