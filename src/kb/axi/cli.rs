//! `rantaiclaw kb` axi-cli subcommand and dispatcher.
//!
//! Implemented in Phase 10 Task 10.2. This file ships as a stub during the
//! TOON-formatter-only commit so the parent `axi` module can re-export
//! `KbCommand` without a `pub use` dangling reference.

// Re-introduced in Task 10.2.
#[derive(clap::Subcommand, Debug)]
pub enum KbCommand {
    /// Placeholder until Task 10.2 lands. Calling `run` returns a typed error.
    #[command(hide = true)]
    Noop,
}

impl KbCommand {
    /// Stubbed dispatcher. Returns a typed error until Task 10.2 fills it in.
    pub async fn run(self) -> crate::kb::KbResult<i32> {
        Err(crate::kb::KbError::Other(
            "kb axi-cli not yet implemented (Task 10.2)".into(),
        ))
    }
}
