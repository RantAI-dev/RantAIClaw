//! `rantaiclaw profile <subcmd>` CLI surface.

use anyhow::Result;
use clap::Subcommand;

use crate::profile::{paths, CloneOpts, ProfileManager};

#[derive(Subcommand, Debug, Clone)]
pub enum ProfileCommand {
    /// List all profiles
    List,

    /// Create a new profile, optionally cloning an existing one
    Create {
        /// Name of the new profile
        name: String,
        /// Source profile to clone from (e.g. `default`)
        #[arg(long)]
        clone: Option<String>,
        /// Also copy the source profile's secrets/ directory
        #[arg(long, default_value_t = false)]
        include_secrets: bool,
        /// Also copy the source profile's memory/ directory
        #[arg(long, default_value_t = false)]
        include_memory: bool,
    },

    /// Switch the active profile
    Use {
        /// Profile name to make active
        name: String,
    },

    /// Clone an existing profile under a new name
    Clone {
        /// Source profile
        src: String,
        /// Destination profile name
        dst: String,
        #[arg(long, default_value_t = false)]
        include_secrets: bool,
        #[arg(long, default_value_t = false)]
        include_memory: bool,
    },

    /// Delete a profile (refuses the active profile unless --force)
    Delete {
        name: String,
        #[arg(long, default_value_t = false)]
        force: bool,
    },

    /// Print the currently active profile name
    Current,
}

pub fn run(cmd: ProfileCommand) -> Result<()> {
    match cmd {
        ProfileCommand::List => {
            let names = ProfileManager::list()?;
            if names.is_empty() {
                println!("(no profiles yet — run `rantaiclaw onboard` to create the default)");
                return Ok(());
            }
            let active = ProfileManager::resolve_active_name();
            for n in names {
                let marker = if n == active { "* " } else { "  " };
                println!("{marker}{n}");
            }
            Ok(())
        }
        ProfileCommand::Create {
            name,
            clone,
            include_secrets,
            include_memory,
        } => {
            let opts = CloneOpts {
                include_secrets,
                include_memory,
            };
            let p = ProfileManager::create(&name, clone.as_deref(), opts)?;
            println!("Created profile {:?} at {}", p.name, p.root.display());
            Ok(())
        }
        ProfileCommand::Use { name } => {
            ProfileManager::use_profile(&name)?;
            println!("Active profile is now {name:?}");
            Ok(())
        }
        ProfileCommand::Clone {
            src,
            dst,
            include_secrets,
            include_memory,
        } => {
            let opts = CloneOpts {
                include_secrets,
                include_memory,
            };
            let p = ProfileManager::create(&dst, Some(&src), opts)?;
            println!(
                "Cloned profile {:?} -> {:?} at {}",
                src,
                p.name,
                p.root.display()
            );
            Ok(())
        }
        ProfileCommand::Delete { name, force } => {
            ProfileManager::delete(&name, force)?;
            println!("Deleted profile {name:?}");
            Ok(())
        }
        ProfileCommand::Current => {
            // Side-effect: ensure the active profile dir exists, so
            // `--profile foo profile current` self-bootstraps `foo` per
            // the agent task verification list.
            let p = ProfileManager::active()?;
            println!("{}", p.name);
            // Touch path so callers can see what's resolved (optional debug).
            let _ = paths::profile_dir(&p.name);
            Ok(())
        }
    }
}
