//! Smart structure-aware document chunker.
//!
//! Port of `src/lib/rag/smart-chunker.ts`. Aware of markdown structure:
//! - preserves code blocks and tables intact
//! - tracks heading hierarchy (`Vec<String>` path) for context
//! - falls back to sentence-grouping when paragraph breaks are absent
//!   (common in PDF extraction)
//!
//! ## UTF-8 / char vs byte note
//!
//! Same as `recursive.rs`: `max_chunk_size` and `overlap_size` are in
//! characters (matching TS semantics). All length comparisons go through
//! [`char_len`]; slicing uses `char_indices` to avoid panicking on
//! multibyte boundaries.
//!
//! ## Sentence regex deviation
//!
//! The TS reference uses `/(?<=[.!?])\s+(?=[A-Z0-9])/`. Rust's `regex`
//! crate does NOT support lookbehind, and pulling in `fancy-regex` just
//! for this one site would add a heavy dep. We hand-roll an equivalent
//! scanner that finds `[.!?]` followed by whitespace followed by an
//! uppercase ASCII letter or digit, then splits the input at that gap.

use regex::Regex;
use std::sync::OnceLock;

use crate::kb::{Chunk, ChunkMetadata};

/// Chunking strategy. Currently the public API exposes the enum for API
/// parity with the TS source even though `chunk()` always uses Smart-mode
/// behaviour (the TS source also routes all four enum values through the
/// same code path with toggleable flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkingStrategy {
    Smart,
    Semantic,
    FixedSize,
    StructureAware,
}

/// Block type for a detected chunk. Mirrors TS `chunkType` literal union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlockType {
    #[default]
    Text,
    Table,
    List,
    Code,
    Heading,
}

/// Smart chunker options.
#[derive(Debug, Clone)]
pub struct SmartChunkOptions {
    /// Maximum characters per chunk.
    pub max_chunk_size: usize,
    /// Overlap (characters) between consecutive chunks.
    pub overlap_size: usize,
    /// Keep triple-backtick code blocks intact.
    pub preserve_code_blocks: bool,
    /// Don't split across headings.
    pub respect_heading_boundaries: bool,
    /// Don't split across sections (heading-text identifies a section).
    pub respect_section_boundaries: bool,
    /// Strategy (kept for API parity with the TS source).
    pub strategy: ChunkingStrategy,
}

impl Default for SmartChunkOptions {
    fn default() -> Self {
        Self {
            max_chunk_size: 800,
            overlap_size: 200,
            preserve_code_blocks: true,
            respect_heading_boundaries: true,
            respect_section_boundaries: true,
            strategy: ChunkingStrategy::Smart,
        }
    }
}

/// Per-chunk metadata produced by the smart chunker. Translated to the
/// canonical [`crate::kb::ChunkMetadata`] by [`smart_chunk_document`].
#[derive(Debug, Clone, Default)]
pub struct SmartChunkMetadata {
    pub chunk_type: BlockType,
    pub document_section: Option<String>,
    pub heading_level: Option<u8>,
    pub hierarchy_path: Option<Vec<String>>,
    pub page_number: Option<u32>,
    pub section: Option<String>,
}

/// One chunk from the smart chunker (pre-conversion to canonical `Chunk`).
#[derive(Debug, Clone)]
pub struct SmartChunk {
    pub chunk_index: usize,
    pub metadata: SmartChunkMetadata,
    pub text: String,
}

/// The smart chunker itself.
pub struct SmartChunker {
    options: SmartChunkOptions,
}

impl SmartChunker {
    pub(crate) fn new(options: SmartChunkOptions) -> Self {
        Self { options }
    }

    /// Synchronous version of TS `chunk(markdown)`. The TS reference returns
    /// a Promise but does no I/O; the only async behaviour is at the
    /// caller level for batching, so we keep this sync.
    pub(crate) fn chunk(&self, markdown: &str) -> Vec<SmartChunk> {
        let mut chunks: Vec<SmartChunk> = Vec::new();
        let mut chunk_index: usize = 0;

        // Split by paragraphs/blocks first.
        let mut blocks = self.split_into_blocks(markdown);

        // If we got very few blocks but lots of text, the content likely
        // lacks paragraph breaks (common with PDF extraction). Fall back
        // to sentence-based splitting.
        let total_length = char_len(markdown);
        let avg_block_size = if blocks.is_empty() {
            total_length
        } else {
            total_length / blocks.len()
        };
        if avg_block_size > self.options.max_chunk_size * 2
            && total_length > self.options.max_chunk_size
        {
            blocks = self.split_by_sentences(markdown);
        }

        let mut current_chunk = String::new();
        let mut current_metadata = SmartChunkMetadata::default();
        let mut current_hierarchy: Vec<String> = Vec::new();

        for block in blocks {
            // Detect structure.
            let mut structure = self.detect_structure(&block);

            // Update hierarchy if heading detected.
            if structure.chunk_type == BlockType::Heading {
                if let Some(level) = structure.heading_level {
                    current_hierarchy = update_hierarchy(&current_hierarchy, &block, level);
                    structure.hierarchy_path = Some(current_hierarchy.clone());
                }
            }

            // Check if new heading/section detected and we respect boundaries.
            let heading_break = self.options.respect_heading_boundaries
                && structure.chunk_type == BlockType::Heading
                && !current_chunk.is_empty();
            let section_break = self.options.respect_section_boundaries
                && structure.section.is_some()
                && structure.section != current_metadata.section
                && !current_chunk.is_empty();

            if heading_break || section_break {
                chunks.push(SmartChunk {
                    chunk_index,
                    metadata: current_metadata.clone(),
                    text: current_chunk.trim().to_string(),
                });
                chunk_index += 1;
                current_chunk.clear();
            }

            // Check if adding this block exceeds max size.
            if char_len(&current_chunk) + char_len(&block) > self.options.max_chunk_size
                && !current_chunk.is_empty()
            {
                chunks.push(SmartChunk {
                    chunk_index,
                    metadata: current_metadata.clone(),
                    text: current_chunk.trim().to_string(),
                });
                chunk_index += 1;

                // Start new chunk with overlap.
                let overlap_text = self.get_overlap(&current_chunk, self.options.overlap_size);
                current_chunk = overlap_text;
            }

            // Add block to current chunk.
            if current_chunk.is_empty() {
                current_chunk.push_str(&block);
            } else {
                current_chunk.push_str("\n\n");
                current_chunk.push_str(&block);
            }

            // Carry hierarchy forward onto non-heading blocks too.
            let hierarchy_path = structure
                .hierarchy_path
                .clone()
                .or_else(|| Some(current_hierarchy.clone()));
            current_metadata = SmartChunkMetadata {
                hierarchy_path,
                ..structure
            };
        }

        // Flush final chunk.
        if !current_chunk.trim().is_empty() {
            chunks.push(SmartChunk {
                chunk_index,
                metadata: current_metadata,
                text: current_chunk.trim().to_string(),
            });
        }

        chunks
    }

    /// Split text into blocks: paragraphs, code blocks, tables.
    fn split_into_blocks(&self, text: &str) -> Vec<String> {
        let mut blocks: Vec<String> = Vec::new();
        let mut current_block = String::new();
        let mut in_code_block = false;
        let mut in_table = false;

        for line in text.split('\n') {
            // Detect code block fence.
            if line.trim_start().starts_with("```") {
                in_code_block = !in_code_block;
                current_block.push_str(line);
                current_block.push('\n');
                if !in_code_block && self.options.preserve_code_blocks {
                    blocks.push(current_block.trim().to_string());
                    current_block.clear();
                }
                continue;
            }

            // Detect table separator line.
            if line.contains('|') && line.contains("---") {
                in_table = true;
            }

            // Inside a code block or table, accumulate verbatim.
            if in_code_block || in_table {
                current_block.push_str(line);
                current_block.push('\n');
                if in_table && !line.contains('|') {
                    in_table = false;
                    blocks.push(current_block.trim().to_string());
                    current_block.clear();
                }
                continue;
            }

            // Regular paragraph handling.
            if line.trim().is_empty() {
                if !current_block.trim().is_empty() {
                    blocks.push(current_block.trim().to_string());
                    current_block.clear();
                }
            } else {
                current_block.push_str(line);
                current_block.push('\n');
            }
        }

        // Flush remaining block.
        if !current_block.trim().is_empty() {
            blocks.push(current_block.trim().to_string());
        }

        blocks.into_iter().filter(|b| !b.is_empty()).collect()
    }

    /// Detect structure in a single block.
    fn detect_structure(&self, block: &str) -> SmartChunkMetadata {
        let mut metadata = SmartChunkMetadata::default();

        // Heading: starts with 1..=6 `#` then space then text.
        if let Some(caps) = heading_regex().captures(block) {
            let hashes = caps.get(1).unwrap().as_str();
            let text = caps.get(2).unwrap().as_str().trim().to_string();
            metadata.chunk_type = BlockType::Heading;
            // hashes.len() is bounded to 1..=6 by the regex, fits in u8.
            metadata.heading_level = u8::try_from(hashes.len()).ok();
            metadata.section = Some(text.clone());
            metadata.document_section = Some(text);
            return metadata;
        }

        // Table: pipe + dashes anywhere.
        if block.contains('|') && block.contains("---") {
            metadata.chunk_type = BlockType::Table;
            return metadata;
        }

        // Code: starts AND ends with triple backtick.
        if block.starts_with("```") && block.ends_with("```") {
            metadata.chunk_type = BlockType::Code;
            return metadata;
        }

        // List: leading list marker.
        if list_regex().is_match(block) {
            metadata.chunk_type = BlockType::List;
            return metadata;
        }

        metadata
    }

    /// Sentence-grouped fallback. Groups detected sentences up to
    /// `max_chunk_size`-character target.
    fn split_by_sentences(&self, text: &str) -> Vec<String> {
        let sentences = split_sentences(text);
        if sentences.len() <= 1 {
            return self.split_by_fixed_size(text);
        }

        let mut blocks: Vec<String> = Vec::new();
        let mut current_block = String::new();
        let target = self.options.max_chunk_size;

        for sentence in sentences {
            if char_len(&current_block) + char_len(&sentence) > target && !current_block.is_empty()
            {
                blocks.push(current_block.trim().to_string());
                current_block = sentence;
            } else if current_block.is_empty() {
                current_block = sentence;
            } else {
                current_block.push(' ');
                current_block.push_str(&sentence);
            }
        }

        if !current_block.trim().is_empty() {
            blocks.push(current_block.trim().to_string());
        }
        blocks
    }

    /// Last-resort fixed-size word-boundary chunker.
    fn split_by_fixed_size(&self, text: &str) -> Vec<String> {
        let mut blocks: Vec<String> = Vec::new();
        let chunk_size = self.options.max_chunk_size;
        let chars: Vec<char> = text.chars().collect();
        let total = chars.len();

        let mut i: usize = 0;
        while i < total {
            let mut end = (i + chunk_size).min(total);

            // Try to break at word boundary if there's more text after.
            if end < total {
                // Find last space in [i, end].
                let mut last_space: Option<usize> = None;
                let mut j = end;
                while j > i {
                    j -= 1;
                    if chars[j] == ' ' {
                        last_space = Some(j);
                        break;
                    }
                }
                if let Some(ls) = last_space {
                    if ls > i + chunk_size / 2 {
                        end = ls;
                    }
                }
            }

            let piece: String = chars[i..end].iter().collect();
            let trimmed = piece.trim().to_string();
            if !trimmed.is_empty() {
                blocks.push(trimmed);
            }
            // Advance past the word-boundary break.
            i = if end == i { i + 1 } else { end };
        }

        blocks
    }

    /// Return up to `size` trailing chars of `text`, snapped to a sentence
    /// boundary if one exists in that window.
    fn get_overlap(&self, text: &str, size: usize) -> String {
        let total = char_len(text);
        if total <= size {
            return text.to_string();
        }
        let skip = total - size;
        let overlap: String = text.chars().skip(skip).collect();
        if let Some(pos) = overlap.rfind(". ") {
            // SAFETY: `pos` is the byte offset returned by rfind(". ") and ". " is pure
            // ASCII (2 bytes), so `pos + 2` is guaranteed to be on a char boundary. The
            // byte slice is intentional here — using char_indices would be wasted work.
            return overlap[pos + 2..].to_string();
        }
        overlap
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

fn char_len(s: &str) -> usize {
    s.chars().count()
}

fn heading_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^(#{1,6})\s+(.+)").expect("heading regex"))
}

fn list_regex() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^[\d*+\.\-]\s+").expect("list regex"))
}

/// Update hierarchy path when a new heading is found. Heading at level N
/// truncates path to N-1 entries then pushes the new heading text.
fn update_hierarchy(current: &[String], heading: &str, level: u8) -> Vec<String> {
    // Strip leading `#`s and whitespace from the raw block text.
    let clean = heading.trim_start_matches('#').trim().to_string();
    let level = level as usize;
    let keep = level.saturating_sub(1);
    let mut new_hierarchy: Vec<String> = current.iter().take(keep).cloned().collect();
    new_hierarchy.push(clean);
    new_hierarchy
}

/// Hand-rolled equivalent of TS regex `(?<=[.!?])\s+(?=[A-Z0-9])`. Splits
/// `text` on whitespace runs between a sentence-terminator and an
/// uppercase-ASCII / digit start. We deliberately avoid pulling in
/// `fancy-regex` for a single use site (the `regex` crate doesn't support
/// lookbehind / lookahead).
fn split_sentences(text: &str) -> Vec<String> {
    let bytes: Vec<char> = text.chars().collect();
    let n = bytes.len();
    let mut out: Vec<String> = Vec::new();
    let mut start: usize = 0;
    let mut i: usize = 0;

    while i < n {
        let c = bytes[i];
        if matches!(c, '.' | '!' | '?') {
            // Look at whitespace run after position i.
            let mut j = i + 1;
            let mut saw_ws = false;
            while j < n && bytes[j].is_whitespace() {
                saw_ws = true;
                j += 1;
            }
            if saw_ws && j < n {
                let next = bytes[j];
                if next.is_ascii_uppercase() || next.is_ascii_digit() {
                    // Split: [start..=i] is one sentence, then skip ws to j.
                    let piece: String = bytes[start..=i].iter().collect();
                    let trimmed = piece.trim().to_string();
                    if !trimmed.is_empty() {
                        out.push(trimmed);
                    }
                    start = j;
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }

    // Trailing piece.
    if start < n {
        let piece: String = bytes[start..].iter().collect();
        let trimmed = piece.trim().to_string();
        if !trimmed.is_empty() {
            out.push(trimmed);
        }
    }

    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

// ---------------------------------------------------------------------------
// Public document-level helpers
// ---------------------------------------------------------------------------

/// Convert a `SmartChunk` to the canonical `Chunk` (drops smart-only fields
/// but populates `section` from either `metadata.section` or
/// `hierarchy_path.join(" > ")`).
fn smart_chunk_to_chunk(
    sc: SmartChunk,
    document_title: &str,
    category: &str,
    subcategory: Option<&str>,
) -> Chunk {
    let section = sc.metadata.section.clone().or_else(|| {
        sc.metadata
            .hierarchy_path
            .as_ref()
            .filter(|path| !path.is_empty())
            .map(|path| path.join(" > "))
    });
    Chunk {
        content: sc.text,
        metadata: ChunkMetadata {
            document_title: document_title.to_string(),
            category: category.to_string(),
            subcategory: subcategory.map(|s| s.to_string()),
            section,
            chunk_index: sc.chunk_index,
            contextual_prefix: None,
        },
    }
}

/// Chunk one document using the smart chunker, returning canonical `Chunk`s.
pub fn smart_chunk_document(
    content: &str,
    document_title: &str,
    category: &str,
    subcategory: Option<&str>,
    options: SmartChunkOptions,
) -> Vec<Chunk> {
    let chunker = SmartChunker::new(options);
    chunker
        .chunk(content)
        .into_iter()
        .map(|sc| smart_chunk_to_chunk(sc, document_title, category, subcategory))
        .filter(|c| !c.content.is_empty())
        .collect()
}

/// Input doc for batch smart-chunking.
pub struct SmartChunkInput<'a> {
    pub content: &'a str,
    pub title: &'a str,
    pub category: &'a str,
    pub subcategory: Option<&'a str>,
}

/// Smart-chunk a batch of documents.
pub fn smart_chunk_documents(
    docs: &[SmartChunkInput<'_>],
    options: SmartChunkOptions,
) -> Vec<Chunk> {
    let mut all: Vec<Chunk> = Vec::new();
    for doc in docs {
        let chunks = smart_chunk_document(
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

/// Raw helper returning `SmartChunk`s with full metadata (no conversion).
pub fn chunk_with_smart_chunker(markdown: &str, options: SmartChunkOptions) -> Vec<SmartChunk> {
    SmartChunker::new(options).chunk(markdown)
}
