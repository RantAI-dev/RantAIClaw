//! `HybridExtractor` — runs a structural extractor (e.g. MinerU) and a
//! text-layer extractor (e.g. unpdf) in parallel and merges their output.
//!
//! Port of `extractors/hybrid-extractor.ts` + `extractors/hybrid-merge.ts`.

use std::time::Instant;

use async_trait::async_trait;

use crate::kb::extract::{elapsed_ms, ExtractionResult, Extractor};
use crate::kb::{KbError, KbResult};

pub struct HybridExtractor {
    name: String,
    structural: Box<dyn Extractor>,
    text_layer: Box<dyn Extractor>,
}

impl HybridExtractor {
    pub fn new(structural: Box<dyn Extractor>, text_layer: Box<dyn Extractor>) -> Self {
        let name = format!("Hybrid({}+{})", structural.name(), text_layer.name());
        Self {
            name,
            structural,
            text_layer,
        }
    }
}

#[async_trait]
impl Extractor for HybridExtractor {
    fn name(&self) -> &str {
        &self.name
    }

    async fn extract(&self, pdf_bytes: &[u8]) -> KbResult<ExtractionResult> {
        let t0 = Instant::now();
        let (s_res, t_res) = tokio::join!(
            self.structural.extract(pdf_bytes),
            self.text_layer.extract(pdf_bytes),
        );

        match (s_res, t_res) {
            (Ok(s), Ok(t)) => {
                let merged = merge_structural_with_text_layer(&s.text, &t.text);
                Ok(ExtractionResult {
                    text: merged,
                    elapsed_ms: elapsed_ms(t0),
                    pages: s.pages.or(t.pages),
                    model: format!("hybrid({}+{})", s.model, t.model),
                    prompt_tokens: s.prompt_tokens.or(t.prompt_tokens),
                    completion_tokens: s.completion_tokens.or(t.completion_tokens),
                    cost_usd: s.cost_usd.or(t.cost_usd),
                })
            }
            (Ok(s), Err(t_err)) => {
                let truncated: String = t_err.to_string().chars().take(100).collect();
                tracing::warn!(
                    text_layer = self.text_layer.name(),
                    "hybrid: text-layer extractor failed ({}); returning structural-only output",
                    truncated
                );
                Ok(s)
            }
            (Err(s_err), Ok(t)) => {
                let truncated: String = s_err.to_string().chars().take(100).collect();
                tracing::warn!(
                    structural = self.structural.name(),
                    "hybrid: structural extractor failed ({}); returning text-layer-only output",
                    truncated
                );
                Ok(t)
            }
            (Err(s_err), Err(t_err)) => {
                let s_msg: String = s_err.to_string().chars().take(150).collect();
                let t_msg: String = t_err.to_string().chars().take(150).collect();
                Err(KbError::Extraction {
                    extractor: self.name.clone(),
                    message: format!(
                        "Both extractors failed — structural({}): {}; textLayer({}): {}",
                        self.structural.name(),
                        s_msg,
                        self.text_layer.name(),
                        t_msg
                    ),
                })
            }
        }
    }
}

// ----------------- Merge algorithm (port of hybrid-merge.ts) -----------------

const ANCHOR_WORDS: usize = 5;
const LENGTH_RATIO_MIN: f64 = 0.7;
const LENGTH_RATIO_MAX: f64 = 1.5;

#[derive(Debug, Clone)]
enum Block {
    Prose(String),
    Heading(String),
    Table(String),
    Code(String),
    Latex(String),
    Blank,
}

fn parse_blocks(md: &str) -> Vec<Block> {
    let lines: Vec<&str> = md.split('\n').collect();
    let mut blocks: Vec<Block> = Vec::new();
    let mut prose: Vec<String> = Vec::new();
    let mut i = 0usize;

    let flush_prose = |prose: &mut Vec<String>, blocks: &mut Vec<Block>| {
        if !prose.is_empty() {
            blocks.push(Block::Prose(prose.join("\n")));
            prose.clear();
        }
    };

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if trimmed.is_empty() {
            flush_prose(&mut prose, &mut blocks);
            blocks.push(Block::Blank);
            i += 1;
            continue;
        }

        // HTML table: <table...> ... </table>, optionally wrapped in a <div>
        let div_table_ahead = trimmed.starts_with("<div")
            && lines.iter().skip(i).take(3).any(|l| l.contains("<table"));
        if trimmed.starts_with("<table") || div_table_ahead {
            flush_prose(&mut prose, &mut blocks);
            let mut buf: Vec<&str> = Vec::new();
            while i < lines.len() {
                buf.push(lines[i]);
                if lines[i].contains("</table>") {
                    i += 1;
                    if i < lines.len() && lines[i].trim().starts_with("</div>") {
                        buf.push(lines[i]);
                        i += 1;
                    }
                    break;
                }
                i += 1;
            }
            blocks.push(Block::Table(buf.join("\n")));
            continue;
        }

        // Markdown pipe table
        if trimmed.starts_with('|') && i + 1 < lines.len() && lines[i + 1].trim().starts_with('|') {
            flush_prose(&mut prose, &mut blocks);
            let mut buf: Vec<&str> = Vec::new();
            while i < lines.len() && lines[i].trim().starts_with('|') {
                buf.push(lines[i]);
                i += 1;
            }
            blocks.push(Block::Table(buf.join("\n")));
            continue;
        }

        // ATX heading
        if is_atx_heading(trimmed) {
            flush_prose(&mut prose, &mut blocks);
            blocks.push(Block::Heading(line.to_string()));
            i += 1;
            continue;
        }

        // Code fence
        if trimmed.starts_with("```") {
            flush_prose(&mut prose, &mut blocks);
            let mut buf: Vec<&str> = vec![lines[i]];
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                buf.push(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                buf.push(lines[i]);
                i += 1;
            }
            blocks.push(Block::Code(buf.join("\n")));
            continue;
        }

        // Block LaTeX: line exactly "$$"
        if trimmed == "$$" {
            flush_prose(&mut prose, &mut blocks);
            let mut buf: Vec<&str> = vec![lines[i]];
            i += 1;
            while i < lines.len() && lines[i].trim() != "$$" {
                buf.push(lines[i]);
                i += 1;
            }
            if i < lines.len() {
                buf.push(lines[i]);
                i += 1;
            }
            blocks.push(Block::Latex(buf.join("\n")));
            continue;
        }

        // Default prose line
        prose.push(line.to_string());
        i += 1;
    }

    flush_prose(&mut prose, &mut blocks);
    blocks
}

fn is_atx_heading(trimmed: &str) -> bool {
    // 1..=6 `#`s, then whitespace, then anything.
    let bytes = trimmed.as_bytes();
    let mut hashes = 0usize;
    while hashes < bytes.len() && hashes < 6 && bytes[hashes] == b'#' {
        hashes += 1;
    }
    if hashes == 0 || hashes >= bytes.len() {
        return false;
    }
    matches!(bytes[hashes], b' ' | b'\t')
}

fn extract_anchor(text: &str, position: Position, word_count: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    if words.len() <= word_count {
        return words.join(" ");
    }
    match position {
        Position::Start => words[..word_count].join(" "),
        Position::End => words[words.len() - word_count..].join(" "),
    }
}

#[derive(Clone, Copy)]
enum Position {
    Start,
    End,
}

fn normalize_space(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Case-insensitive, whitespace-normalized substring search.
///
/// Returns a CHAR offset (not a byte offset) into `haystack_lc`, where
/// `haystack_lc = normalized_haystack.to_lowercase()`. Callers must convert
/// that char offset back to a byte offset in the ORIGINAL normalized string
/// via `char_indices()` before slicing — a byte offset from the lowercased
/// copy is not generally valid in the original, because `to_lowercase()`
/// can change byte length (e.g. Turkish `İ` -> `i̇`, German `ß` -> `ss`).
fn find_normalized(haystack_lc: &str, needle: &str, from_chars: usize) -> Option<usize> {
    let norm_needle = normalize_space(needle).to_lowercase();
    if norm_needle.is_empty() {
        return None;
    }
    // Translate the caller's char-offset into a byte-offset within haystack_lc.
    let from_bytes = match haystack_lc.char_indices().nth(from_chars) {
        Some((b, _)) => b,
        None if from_chars == haystack_lc.chars().count() => haystack_lc.len(),
        None => return None,
    };
    let rel_bytes = haystack_lc[from_bytes..].find(&norm_needle)?;
    let abs_bytes = from_bytes + rel_bytes;
    // Char count up to the match start.
    Some(haystack_lc[..abs_bytes].chars().count())
}

fn try_substitute(prose_raw: &str, normalized_text_layer: &str) -> Option<String> {
    let prose = prose_raw.trim();
    let start_anchor = extract_anchor(prose, Position::Start, ANCHOR_WORDS);
    let end_anchor = extract_anchor(prose, Position::End, ANCHOR_WORDS);
    if start_anchor.is_empty() || end_anchor.is_empty() {
        return None;
    }

    let haystack_lc = normalized_text_layer.to_lowercase();

    // All offsets below are CHAR offsets into haystack_lc / normalized_text_layer.
    // If the lowercased copy has a different char count than the original
    // (e.g. `ß` -> `ss` adds one char), we can't safely map back, so bail.
    let original_char_count = normalized_text_layer.chars().count();
    let lc_char_count = haystack_lc.chars().count();
    if original_char_count != lc_char_count {
        return None;
    }

    let start_chars = find_normalized(&haystack_lc, &start_anchor, 0)?;
    let start_anchor_lc_chars = normalize_space(&start_anchor)
        .to_lowercase()
        .chars()
        .count();
    let end_chars = find_normalized(
        &haystack_lc,
        &end_anchor,
        start_chars + start_anchor_lc_chars,
    )?;
    let end_anchor_lc_chars = normalize_space(&end_anchor).to_lowercase().chars().count();
    let span_end_chars = (end_chars + end_anchor_lc_chars).min(original_char_count);

    // Map char offsets back to byte offsets in the ORIGINAL normalized string.
    // We collect char_indices once; the buffer is bounded by the prose+layer
    // sizes already kept in memory by the caller, so this is fine.
    let char_byte_index: Vec<usize> = normalized_text_layer
        .char_indices()
        .map(|(b, _)| b)
        .chain(std::iter::once(normalized_text_layer.len()))
        .collect();
    let start_byte = *char_byte_index.get(start_chars)?;
    let end_byte = *char_byte_index.get(span_end_chars)?;
    let span = &normalized_text_layer[start_byte..end_byte];

    let ratio = span.len() as f64 / prose.len().max(1) as f64;
    if !(LENGTH_RATIO_MIN..=LENGTH_RATIO_MAX).contains(&ratio) {
        return None;
    }
    Some(span.to_string())
}

/// Merge MinerU-style structural markdown with unpdf-style text-layer prose.
///
/// Strategy: keep ALL structural blocks (tables, headings, code, latex)
/// verbatim. For prose blocks, attempt to substitute the text-layer's
/// character-perfect span when its anchors resolve cleanly and the length
/// ratio is within `[LENGTH_RATIO_MIN, LENGTH_RATIO_MAX]`.
pub fn merge_structural_with_text_layer(structural: &str, text_layer: &str) -> String {
    if text_layer.trim().is_empty() {
        return structural.to_string();
    }
    if structural.trim().is_empty() {
        return text_layer.to_string();
    }
    let normalized = normalize_space(text_layer);
    let blocks = parse_blocks(structural);
    let mut out: Vec<String> = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            Block::Blank => out.push(String::new()),
            Block::Prose(raw) => {
                let sub = try_substitute(&raw, &normalized);
                out.push(sub.unwrap_or(raw));
            }
            Block::Heading(raw) | Block::Table(raw) | Block::Code(raw) | Block::Latex(raw) => {
                out.push(raw);
            }
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_prose_from_text_layer() {
        let structural = "# Heading\n\nQuick brown fox jumps over the lazy dog.";
        let text_layer = "Quick brown fox jumps over the lazy dog.";
        let merged = merge_structural_with_text_layer(structural, text_layer);
        assert!(merged.contains("# Heading"));
        assert!(merged.contains("Quick brown fox"));
    }

    #[test]
    fn keeps_table_block_verbatim() {
        let structural = "| a | b |\n|---|---|\n| 1 | 2 |";
        let text_layer = "a b 1 2";
        let merged = merge_structural_with_text_layer(structural, text_layer);
        assert!(merged.contains("| a | b |"));
    }
}
