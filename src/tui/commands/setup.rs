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
            // Provisioner first: `runtime` and `hardware` name both a
            // provisioner and a category, and they have always opened the
            // provisioner. Category resolution is a fallback, so nothing that
            // works today changes — only args that used to error now resolve.
            if crate::onboard::provision::provisioner_for(arg).is_none() {
                if let Some(cat) = category_from_arg(arg) {
                    return Ok(CommandResult::OpenSetupCategory {
                        category: category_key(cat).to_string(),
                    });
                }
            }
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

/// Resolve a `/setup <arg>` to a category, accepting the plural the UI prints.
///
/// `category_from_key` only knows the canonical singular keys, and it was only
/// ever reachable from the picker's `cat:` drill-down — the arg path never
/// consulted it, so `/setup core|channel|integration|routing` all died with
/// "unknown provisioner". Meanwhile the startup banner, `/channels`' footer
/// and its empty state all tell the user to run `/setup channels`, and
/// `docs/reference/commands.md` documents `rantaiclaw setup channels`. Accept
/// the plural rather than making four call sites lie in the singular.
///
/// `runtime` and `hardware` are BOTH provisioner names and category names.
/// Callers resolve provisioners first so those keep opening the provisioner
/// they open today; this is a fallback for args that match nothing else.
pub fn category_from_arg(arg: &str) -> Option<ProvisionerCategory> {
    let lower = arg.trim().to_ascii_lowercase();
    category_from_key(&lower).or_else(|| {
        lower
            .strip_suffix('s')
            .and_then(|singular| category_from_key(singular))
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The four category names that used to die with "unknown provisioner",
    /// plus the plural the banner/footer/docs actually tell users to type.
    #[test]
    fn category_args_that_used_to_error_now_resolve() {
        for (arg, want) in [
            ("core", ProvisionerCategory::Core),
            ("channel", ProvisionerCategory::Channel),
            ("channels", ProvisionerCategory::Channel),
            ("integration", ProvisionerCategory::Integration),
            ("integrations", ProvisionerCategory::Integration),
            ("routing", ProvisionerCategory::Routing),
        ] {
            assert_eq!(category_from_arg(arg), Some(want), "arg {arg:?}");
        }
    }

    #[test]
    fn category_args_are_case_and_space_insensitive() {
        assert_eq!(
            category_from_arg("  Channels "),
            Some(ProvisionerCategory::Channel)
        );
        assert_eq!(category_from_arg("CORE"), Some(ProvisionerCategory::Core));
    }

    #[test]
    fn non_category_args_do_not_resolve() {
        for arg in ["telegram", "provider", "knowledge", "", "nonsense"] {
            assert_eq!(category_from_arg(arg), None, "arg {arg:?}");
        }
    }

    /// `runtime` and `hardware` name both a provisioner and a category. They
    /// have always opened the provisioner, and the dispatcher resolves
    /// provisioners first so they still do — this pins that the fallback is
    /// additive, not a behavior change.
    #[test]
    fn runtime_and_hardware_still_resolve_as_provisioners_first() {
        for name in ["runtime", "hardware"] {
            assert!(
                crate::onboard::provision::provisioner_for(name).is_some(),
                "{name} must stay a provisioner"
            );
            // They also name categories — which is exactly why order matters.
            assert!(
                category_from_arg(name).is_some(),
                "{name} also names a category"
            );
        }
    }

    #[test]
    fn category_key_round_trips_through_category_from_arg() {
        for cat in [
            ProvisionerCategory::Core,
            ProvisionerCategory::Channel,
            ProvisionerCategory::Integration,
            ProvisionerCategory::Runtime,
            ProvisionerCategory::Hardware,
            ProvisionerCategory::Routing,
        ] {
            assert_eq!(category_from_arg(category_key(cat)), Some(cat));
        }
    }
}
