//! Recursive separator-based text chunker.
//!
//! Port of `src/lib/rag/chunker.ts` from the TypeScript source. Splits a
//! document into overlapping chunks using a priority list of separators —
//! biggest semantic unit first (markdown headings) down to single spaces.
//!
//! ## UTF-8 / char vs byte note
//!
//! The TS source treats `chunk_size` as a character count (JS `String.length`
//! is UTF-16 code units, but for ASCII-heavy text behaves like char-count).
//! Rust `&str` is byte-indexed, so we use `.chars().count()` for size checks
//! and `.char_indices()` for slicing to preserve parity. This is slower than
//! byte slicing but correctness on multibyte input outweighs that here.

use crate::kb::{Chunk, ChunkMetadata};

/// Default chunk size in characters.
const DEFAULT_CHUNK_SIZE: usize = 1000;
/// Default overlap in characters.
const DEFAULT_CHUNK_OVERLAP: usize = 200;
/// Default separators in priority order. Largest semantic unit first.
const DEFAULT_SEPARATORS: &[&str] = &["\n## ", "\n### ", "\n#### ", "\n\n", "\n", ". ", " "];

/// Options for the recursive chunker.
#[derive(Debug, Clone)]
pub struct ChunkOptions {
    /// Target size of each chunk, in characters.
    pub chunk_size: usize,
    /// Overlap (characters) between consecutive chunks.
    pub chunk_overlap: usize,
    /// Separators in priority order (largest semantic unit first).
    pub separators: Vec<&'static str>,
}

impl Default for ChunkOptions {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            chunk_overlap: DEFAULT_CHUNK_OVERLAP,
            separators: DEFAULT_SEPARATORS.to_vec(),
        }
    }
}

/// Count characters (not bytes) in a `&str`.
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Take the last `n` characters of `s` as an owned `String`.
fn take_last_chars(s: &str, n: usize) -> String {
    let total = char_len(s);
    if n >= total {
        return s.to_string();
    }
    let skip = total - n;
    s.chars().skip(skip).collect()
}

/// Recursive descent through the separator list. Each level splits on its
/// separator; if a piece is still too big, the next call drops the head of
/// the separator list and retries on the leftover separators.
fn recursive_split(text: &str, separators: &[&str], chunk_size: usize) -> Vec<String> {
    if char_len(text) <= chunk_size || separators.is_empty() {
        return vec![text.to_string()];
    }

    let separator = separators[0];
    let remaining = &separators[1..];

    let splits: Vec<&str> = text.split(separator).collect();
    let mut chunks: Vec<String> = Vec::new();
    let mut current_chunk = String::new();

    for split in splits {
        let potential_chunk = if current_chunk.is_empty() {
            split.to_string()
        } else {
            format!("{current_chunk}{separator}{split}")
        };

        if char_len(&potential_chunk) <= chunk_size {
            current_chunk = potential_chunk;
        } else {
            if !current_chunk.is_empty() {
                chunks.push(std::mem::take(&mut current_chunk));
            }

            // If the split itself is too large, recurse with the rest of the
            // separator list to break it down further.
            if char_len(split) > chunk_size {
                let sub_chunks = recursive_split(split, remaining, chunk_size);
                chunks.extend(sub_chunks);
                current_chunk.clear();
            } else {
                current_chunk = split.to_string();
            }
        }
    }

    if !current_chunk.is_empty() {
        chunks.push(current_chunk);
    }

    chunks
}

/// Add character-overlap to consecutive chunks. Each chunk after index 0
/// gets the trailing `overlap` chars of the previous chunk prepended.
fn add_overlap(chunks: Vec<String>, overlap: usize) -> Vec<String> {
    if overlap == 0 || chunks.len() <= 1 {
        return chunks;
    }

    let mut result: Vec<String> = Vec::with_capacity(chunks.len());

    for (i, chunk) in chunks.iter().enumerate() {
        if i == 0 {
            result.push(chunk.clone());
        } else {
            let prev = &chunks[i - 1];
            let overlap_text = take_last_chars(prev, overlap);
            result.push(format!("{overlap_text}{chunk}"));
        }
    }

    result
}

/// Extract the first markdown section header (lines starting with `#+`) and
/// return the header text after the `#`s.
fn extract_section_header(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            let after_hashes = trimmed.trim_start_matches('#');
            // require at least one space between hashes and the title text
            if let Some(rest) = after_hashes.strip_prefix(' ') {
                let header = rest.trim_end();
                if !header.is_empty() {
                    return Some(header.to_string());
                }
            }
        }
    }
    None
}

/// Chunk a single document. Empty / whitespace-only chunks are filtered out
/// before returning, matching the TS reference behaviour.
pub fn chunk_document(
    content: &str,
    document_title: &str,
    category: &str,
    subcategory: Option<&str>,
    options: ChunkOptions,
) -> Vec<Chunk> {
    let raw_chunks = recursive_split(content, &options.separators, options.chunk_size);
    let overlapped = add_overlap(raw_chunks, options.chunk_overlap);

    let mut chunks: Vec<Chunk> = Vec::with_capacity(overlapped.len());
    for (index, body) in overlapped.into_iter().enumerate() {
        let section = extract_section_header(&body);
        let trimmed = body.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        chunks.push(Chunk {
            content: trimmed,
            metadata: ChunkMetadata {
                document_title: document_title.to_string(),
                category: category.to_string(),
                subcategory: subcategory.map(|s| s.to_string()),
                section,
                chunk_index: index,
                contextual_prefix: None,
            },
        });
    }

    chunks
}

/// Input document for [`chunk_documents`].
pub struct ChunkInput<'a> {
    pub content: &'a str,
    pub title: &'a str,
    pub category: &'a str,
    pub subcategory: Option<&'a str>,
}

/// Chunk multiple documents in sequence, returning all chunks concatenated.
pub fn chunk_documents(documents: &[ChunkInput<'_>], options: ChunkOptions) -> Vec<Chunk> {
    let mut all: Vec<Chunk> = Vec::new();
    for doc in documents {
        let chunks = chunk_document(
            doc.content,
            doc.title,
            doc.category,
            doc.subcategory,
            options.clone(),
        );
        all.extend(chunks);
    }
    all
}
