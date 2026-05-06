use anyhow::Result;

use super::{CommandHandler, CommandResult};
use crate::onboard::provision::{available, provisioner_for, ProvisionerCategory};
use crate::tui::context::TuiContext;
use crate::tui::widgets::{ListPicker, ListPickerItem, ListPickerKind};

pub struct SetupCommand;

impl CommandHandler for SetupCommand {
    fn name(&self) -> &str {
        "setup"
    }

    fn description(&self) -> &str {
        "Configure providers, channels, and integrations"
    }

    fn usage(&self) -> &str {
        "setup [topic|full]"
    }

    fn aliases(&self) -> Vec<&str> {
        vec!["wizard"]
    }

    fn execute(&self, args: &str, _ctx: &mut TuiContext) -> Result<CommandResult> {
        let arg = args.trim();
        if arg.eq_ignore_ascii_case("full") {
            return Ok(CommandResult::OpenFirstRunWizard);
        }
        if !arg.is_empty() {
            return Ok(CommandResult::OpenSetupOverlay {
                provisioner: Some(arg.to_string()),
            });
        }

        // Top picker shows ONE entry per category. Six items, no
        // pagination, no in-list section headers — drill down into a
        // sub-picker on Enter. Replaces the previous 41-item flat list
        // that paginated awkwardly across 9 pages.
        let all = available();

        let mut categories: Vec<ProvisionerCategory> = Vec::new();
        for (name, _) in &all {
            let cat = provisioner_for(name)
                .map(|p| p.category())
                .unwrap_or(ProvisionerCategory::Core);
            if !categories.contains(&cat) {
                categories.push(cat);
            }
        }
        categories.sort_by_key(|c| cat_order(*c));

        let items: Vec<ListPickerItem> = categories
            .into_iter()
            .map(|cat| {
                let cat_items: Vec<&str> = all
                    .iter()
                    .filter_map(|(name, _)| {
                        let c = provisioner_for(name)
                            .map(|p| p.category())
                            .unwrap_or(ProvisionerCategory::Core);
                        (c == cat).then_some(*name)
                    })
                    .collect();

                // Show count + a teaser of the first 4 names.
                let count = cat_items.len();
                let teaser = {
                    let mut shown: Vec<&str> = cat_items.iter().take(4).copied().collect();
                    if cat_items.len() > 4 {
                        shown.push("…");
                    }
                    shown.join(", ")
                };
                let secondary = format!(
                    "{count} {} · {teaser}",
                    if count == 1 { "item" } else { "items" }
                );

                ListPickerItem {
                    key: format!("cat:{}", category_key(cat)),
                    primary: cat_label(cat).to_string(),
                    secondary,
                }
            })
            .collect();

        let picker = ListPicker::new(
            ListPickerKind::SetupTopic,
            "Setup",
            items,
            None,
            "no setup categories available",
        );
        Ok(CommandResult::OpenListPicker(picker))
    }
}

pub fn cat_label(c: ProvisionerCategory) -> &'static str {
    match c {
        ProvisionerCategory::Core => "Core",
        ProvisionerCategory::Channel => "Channels",
        ProvisionerCategory::Integration => "Integrations",
        ProvisionerCategory::Runtime => "Runtime",
        ProvisionerCategory::Hardware => "Hardware",
        ProvisionerCategory::Routing => "Routing",
    }
}

pub fn category_key(c: ProvisionerCategory) -> &'static str {
    match c {
        ProvisionerCategory::Core => "core",
        ProvisionerCategory::Channel => "channel",
        ProvisionerCategory::Integration => "integration",
        ProvisionerCategory::Runtime => "runtime",
        ProvisionerCategory::Hardware => "hardware",
        ProvisionerCategory::Routing => "routing",
    }
}

pub fn category_from_key(key: &str) -> Option<ProvisionerCategory> {
    match key {
        "core" => Some(ProvisionerCategory::Core),
        "channel" => Some(ProvisionerCategory::Channel),
        "integration" => Some(ProvisionerCategory::Integration),
        "runtime" => Some(ProvisionerCategory::Runtime),
        "hardware" => Some(ProvisionerCategory::Hardware),
        "routing" => Some(ProvisionerCategory::Routing),
        _ => None,
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
