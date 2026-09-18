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
                    // The Channels category counts what a user can actually
                    // set up here. The locked channels still exist as their
                    // own dimmed rows one level down; a count that included
                    // them would promise more than `/setup channels` opens.
                    .filter(|name| {
                        cat != ProvisionerCategory::Channel
                            || crate::channels::channel_is_usable(
                                crate::channels::catalog_key_for_provisioner(name),
                            )
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
                    disabled: false,
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
    category_from_key(&lower).or_else(|| lower.strip_suffix('s').and_then(category_from_key))
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

/// The channel sub-picker's rows: usable provisioners first, then — only if
/// any exist — a static "Under development" heading followed by the locked
/// ones, dimmed. Derived from the provisioner registry and the catalog on
/// every call, so a channel added to one and not the other shows up here as
/// wrong rather than as a silent omission.
pub fn channel_picker_entries() -> Vec<crate::tui::widgets::ListPickerEntry> {
    use crate::onboard::provision::{available, provisioner_for};
    use crate::tui::widgets::{ListPickerEntry, ListPickerItem};

    let mut usable = Vec::new();
    let mut locked = Vec::new();
    for (name, desc) in available() {
        let Some(p) = provisioner_for(name) else {
            continue;
        };
        if p.category() != ProvisionerCategory::Channel {
            continue;
        }
        let catalog_key = crate::channels::catalog_key_for_provisioner(name);
        let is_usable = crate::channels::channel_is_usable(catalog_key);
        let entry = ListPickerEntry::Item(ListPickerItem {
            key: name.to_string(),
            primary: name.to_string(),
            secondary: desc.to_string(),
            disabled: !is_usable,
        });
        if is_usable {
            usable.push(entry);
        } else {
            locked.push(entry);
        }
    }
    let mut entries = usable;
    if !locked.is_empty() {
        entries.push(ListPickerEntry::static_heading("Under development"));
        entries.extend(locked);
    }
    entries
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

    // The resolver tests above prove `category_from_arg`/`provisioner_for` map
    // correctly, but nothing exercised `SetupCommand::execute` itself — the
    // routing from a raw arg to a `CommandResult` variant. These pin it.

    #[test]
    fn execute_full_opens_the_first_run_wizard() {
        let (mut ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let result = SetupCommand.execute("full", &mut ctx).unwrap();
        assert!(
            matches!(result, CommandResult::OpenFirstRunWizard),
            "`/setup full` must open the wizard, got {result:?}"
        );
    }

    #[test]
    fn execute_provisioner_arg_opens_the_overlay() {
        // `runtime` names both a provisioner and a category; provisioner-first
        // resolution opens the overlay for it.
        let (mut ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let result = SetupCommand.execute("runtime", &mut ctx).unwrap();
        match result {
            CommandResult::OpenSetupOverlay { provisioner } => {
                assert_eq!(provisioner.as_deref(), Some("runtime"));
            }
            other => panic!("expected OpenSetupOverlay, got {other:?}"),
        }
    }

    #[test]
    fn top_picker_channels_row_counts_only_usable_channels() {
        // The Channels category in the top `/setup` picker used to count
        // every channel provisioner, locked or not — reading "17 items" when
        // only six can actually be opened here. It must count (and list)
        // only what `channel_is_usable` allows.
        let (mut ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let result = SetupCommand.execute("", &mut ctx).unwrap();
        let CommandResult::OpenListPicker(picker) = result else {
            panic!("expected OpenListPicker, got {result:?}");
        };
        let usable_channel_count = crate::onboard::provision::available()
            .into_iter()
            .filter(|(name, _)| {
                crate::onboard::provision::provisioner_for(name)
                    .is_some_and(|p| p.category() == ProvisionerCategory::Channel)
                    && crate::channels::channel_is_usable(
                        crate::channels::catalog_key_for_provisioner(name),
                    )
            })
            .count();
        let channel_row_key = format!("cat:{}", category_key(ProvisionerCategory::Channel));
        let channels_row = picker
            .entries()
            .iter()
            .find_map(|e| e.as_item().filter(|i| i.key == channel_row_key))
            .expect("a Channels row in the top picker");
        assert!(
            channels_row
                .secondary
                .starts_with(&format!("{usable_channel_count} ")),
            "expected the count {usable_channel_count} in {:?}",
            channels_row.secondary
        );
    }

    #[test]
    fn execute_category_only_arg_opens_the_category() {
        // `channels` is a category but not a provisioner, so it falls through to
        // the category overlay (the arg that used to error).
        let (mut ctx, _req_rx, _events_tx) = TuiContext::test_context();
        let result = SetupCommand.execute("channels", &mut ctx).unwrap();
        match result {
            CommandResult::OpenSetupCategory { category } => {
                assert_eq!(category, category_key(ProvisionerCategory::Channel));
            }
            other => panic!("expected OpenSetupCategory, got {other:?}"),
        }
    }

    /// Every disabled state in `channel_picker_entries` must come from
    /// `channel_is_usable`, derived by iterating the provisioner registry —
    /// never a name literal in this test. A provisioner name with no entry
    /// in `catalog_key_for_provisioner` falls through to itself, and
    /// `channel_is_usable` answers `false` for any key `CHANNEL_CATALOG`
    /// does not carry — so a provisioner nobody mapped reads as locked, not
    /// as usable, and this test would catch that disagreement too.
    #[test]
    fn channel_picker_disabled_state_matches_channel_is_usable_for_the_whole_registry() {
        use crate::onboard::provision::{available, provisioner_for};

        let entries = channel_picker_entries();
        let channel_names: std::collections::HashSet<&str> = available()
            .into_iter()
            .filter_map(|(name, _)| {
                provisioner_for(name)
                    .and_then(|p| (p.category() == ProvisionerCategory::Channel).then_some(name))
            })
            .collect();

        // Every channel provisioner appears exactly once, as an Item (never
        // as the heading).
        let item_keys: Vec<&str> = entries
            .iter()
            .filter_map(|e| e.as_item().map(|i| i.key.as_str()))
            .collect();
        for name in &channel_names {
            assert!(
                item_keys.contains(name),
                "{name} is a channel provisioner but is missing from the picker"
            );
        }
        assert_eq!(
            item_keys.len(),
            channel_names.len(),
            "the picker must not add or drop channel rows"
        );

        for entry in &entries {
            let Some(item) = entry.as_item() else {
                continue;
            };
            let catalog_key = crate::channels::catalog_key_for_provisioner(&item.key);
            assert_eq!(
                item.disabled,
                !crate::channels::channel_is_usable(catalog_key),
                "{}'s disabled flag disagrees with channel_is_usable",
                item.key
            );
        }
    }

    #[test]
    fn usable_channel_rows_come_before_the_heading_which_comes_before_locked_rows() {
        let entries = channel_picker_entries();
        let mut seen_heading = false;
        for entry in &entries {
            match entry {
                crate::tui::widgets::ListPickerEntry::StaticHeading { label } => {
                    assert_eq!(label, "Under development");
                    assert!(!seen_heading, "the heading must appear at most once");
                    seen_heading = true;
                }
                crate::tui::widgets::ListPickerEntry::Item(item) => {
                    assert_eq!(
                        item.disabled, seen_heading,
                        "row {} is on the wrong side of the heading",
                        item.key
                    );
                }
                crate::tui::widgets::ListPickerEntry::CategoryHeader { .. } => {
                    panic!("the channel picker does not use collapsible categories")
                }
            }
        }
    }
}
