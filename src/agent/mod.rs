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
mod tests;

#[allow(unused_imports)]
pub use agent::{Agent, AgentBuilder};
#[allow(unused_imports)]
pub use events::{AgentEvent, AgentEventSender, TurnResult};
#[allow(unused_imports)]
pub use loop_::{process_message, run};
