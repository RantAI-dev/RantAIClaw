//! Format a [`RetrievalResult`] for inclusion in an LLM prompt — port of
//! `formatContextForPrompt` in `src/lib/rag/retriever.ts:234-261`.
//!
//! The instruction block is intentionally verbose: a generic "you are helpful"
//! system prompt biases models toward terseness, so for RAG turns we restate
//! the rules locally — be thorough, cite, and refuse cleanly when the context
//! falls short. The exact wording is load-bearing; do NOT trim or paraphrase
//! without re-running the evals at `tests/fixtures/rag-golden.json`.

use crate::kb::retrieve::RetrievalResult;

/// Build the prompt fragment. Returns an empty string when `result.context`
/// is empty — caller can branch on `.is_empty()` to decide whether to inject
/// any KB context at all.
pub fn format_context_for_prompt(result: &RetrievalResult) -> String {
    if result.context.is_empty() {
        return String::new();
    }

    let source_list: String = result
        .sources
        .iter()
        .map(|s| match &s.section {
            Some(section) => format!("- {}: {}", s.document_title, section),
            None => format!("- {}", s.document_title),
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Verbatim from `retriever.ts:243-260`. The `\n` after the opening
    // backtick + before the closing backtick in the TS source produces a
    // leading and trailing blank line that `.trim()` strips; we reproduce
    // by building without those wrapping blanks (i.e. start with "## …"
    // directly and end after the last newline of `sourceList`).
    format!(
        "## Knowledge Base Context\n\
\n\
The excerpts below are your primary source for this question.\n\
\n\
When answering:\n\
- Treat the excerpts as the source of truth for specific facts. Every concrete claim (definitions, paragraph numbers, effective dates, scope rules, exclusions, numerical thresholds) MUST come from the excerpts and be cited.\n\
- You MAY add brief background context (1-2 sentences) to frame an answer when essential for understanding — but mark it as framing, not fact. Never substitute general knowledge for an absent specific detail.\n\
- Cite each factual claim inline using `[Document Title — Section]` (or `[Document Title]` when no section is given). Match the list below.\n\
- Be thorough within the excerpts. Cover every aspect the excerpts support; do not invent aspects they do not mention.\n\
- If a specific detail the user asked for is not in the excerpts, say so explicitly (\"not specified in the available excerpts\") rather than guessing.\n\
\n\
Excerpts:\n\
{}\n\
\n\
Sources:\n\
{}",
        result.context, source_list,
    )
}
