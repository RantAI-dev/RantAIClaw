//! Stub trait used by Wave 2C/2D/2E section files.
//! Wave 3 replaces this with the real trait at `src/onboard/section/mod.rs`.
use anyhow::Result;
use crate::profile::Profile;
use crate::config::Config;

pub struct SetupContext<'a> {
    pub profile: &'a Profile,
    pub config: &'a mut Config,
    pub interactive: bool,
}

pub trait SetupSection {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn is_already_configured(&self, profile: &Profile, config: &Config) -> bool;
    fn run(&self, ctx: &mut SetupContext) -> Result<()>;
    fn headless_hint(&self) -> &'static str;
}
