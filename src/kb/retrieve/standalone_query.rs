//! Standalone query rewriter — stub. Task 7.5 ports `standalone-query.ts`.

use crate::kb::{KbConfig, KbResult};

/// Returns the original query. Task 7.5 will replace this with a real
/// OpenRouter-backed multi-turn query rewriter.
pub async fn rewrite_standalone(
    _cfg: &KbConfig,
    user_query: &str,
    _chat_history: &[(String, String)],
) -> KbResult<String> {
    Ok(user_query.to_string())
}
