//! Format retrieval result for inclusion in an LLM prompt — stub.
//! Task 7.5 will port `retriever.ts:230-261`.

use crate::kb::retrieve::RetrievalResult;

/// Empty stub. Real implementation in Task 7.5 wraps `result.context` in the
/// "## Knowledge Base Context" instruction block and source list.
pub fn format_context_for_prompt(_result: &RetrievalResult) -> String {
    String::new()
}
