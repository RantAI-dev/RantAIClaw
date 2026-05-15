//! AXI surface for the Knowledge Base.
//!
//! AXI (Agent eXecution Interface) is the family of agent-shellable CLI tools
//! that wrap the in-process KB API with token-efficient TOON output, plus
//! structured error reporting suitable for an LLM caller. Per `axi.md`:
//!
//! - **TOON**: a compact tabular notation cheaper than JSON for LLM context.
//! - **Idempotent + non-interactive**: agents shell out, never prompted.
//! - **stdout for everything**: success rows AND errors. Exit codes (0/1)
//!   carry the success signal; stderr is reserved for nothing today.
//!
//! Sub-modules:
//! - [`toon`] — the TOON formatter.
//! - [`cli`] — the `rantaiclaw kb ...` clap subcommand and dispatcher.
//! - [`api`] — the `/api/v1/kb/*` axum router merged into the gateway.

pub mod api;
pub mod cli;
pub mod toon;

pub use api::router;
pub use cli::KbCommand;
pub use toon::{format_toon, serialize_value};
