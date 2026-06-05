//! Embedding-time metadata prefix builder.
//!
//! Port of `src/lib/rag/chunker.ts:184-195`. Prepends a small block of
//! metadata (`Category:`, `Topic:`, `Section:`, `Context:`) to the chunk
//! body so the embedding sees the surrounding semantic context. Each line
//! after `Category:` is conditional on the corresponding metadata being
//! `Some(_)`; `contextualPrefix` is additionally skipped when it trims to
//! empty.

use crate::kb::Chunk;

/// Build the embedding-ready text for a chunk by prepending metadata
/// lines. Layout:
///
/// ```text
/// Category: {category}
/// Topic: {subcategory}        (optional)
/// Section: {section}          (optional)
/// Context: {contextual_prefix}  (optional)
///
/// {chunk content}
/// ```
pub fn prepare_chunk_for_embedding(chunk: &Chunk) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(5);
    parts.push(format!("Category: {}", chunk.metadata.category));
    if let Some(sub) = &chunk.metadata.subcategory {
        parts.push(format!("Topic: {sub}"));
    }
    if let Some(section) = &chunk.metadata.section {
        parts.push(format!("Section: {section}"));
    }
    if let Some(prefix) = &chunk.metadata.contextual_prefix {
        let trimmed = prefix.trim();
        if !trimmed.is_empty() {
            parts.push(format!("Context: {trimmed}"));
        }
    }
    parts.push(String::new());
    parts.push(chunk.content.clone());
    parts.join("\n")
}
