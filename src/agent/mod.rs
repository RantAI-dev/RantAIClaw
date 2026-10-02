#[allow(clippy::module_inception)]
pub mod agent;
pub mod classifier;
pub mod compaction;
pub mod dispatcher;
pub mod events;
pub mod loop_;
pub mod memory_loader;
pub mod prompt;

#[cfg(test)]
pub(crate) mod door_test_support;
#[cfg(test)]
mod memory_view_doors_tests;
#[cfg(test)]
mod prompt_tools_doors_tests;
#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub use agent::{Agent, AgentBuilder, NoModelConfigured};
#[allow(unused_imports)]
pub use events::{AgentEvent, AgentEventSender, TurnResult};
#[allow(unused_imports)]
pub use loop_::{run, run_with_scope};
