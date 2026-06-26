//! Heuristics that classify a `pdf-extract` (unpdf) text-layer output as
//! "sufficient for retrieval" or "needs OCR fallback".
//!
//! Port of `extractors/text-layer-signals.ts`. Every function is a pure
//! predicate; thresholds match the TS source.

/// Tunable thresholds. Defaults match `DEFAULT_ROUTER_OPTS` in TS.
#[derive(Debug, Clone, Copy)]
pub struct RouterOpts {
    /// Minimum text-layer characters PER PDF page that we trust as "real
    /// content". Lower than this signals a scanned / image-only doc.
    pub min_chars_per_page: usize,
    /// Maximum lines that look columnar (table flattened by the text-layer).
    /// Above this → route to OCR to preserve table structure.
    pub max_columnar_lines: usize,
    /// Maximum `$X,XXX` currency patterns. Financial tables exceed this
    /// quickly; plain prose rarely does.
    pub max_currency_matches: usize,
    /// Minimum ratio of extracted-text bytes to source-file bytes below which a
    /// large PDF is treated as image/design-heavy (the real content lives in
    /// pictures the text layer can't read) and routed to OCR. Only applied to
    /// files above `SIZE_FLOOR_BYTES`. Default 0.005 (0.5%).
    pub min_text_filesize_ratio: f32,
}

impl Default for RouterOpts {
    fn default() -> Self {
        Self {
            min_chars_per_page: 300,
            max_columnar_lines: 5,
            max_currency_matches: 10,
            min_text_filesize_ratio: 0.005,
        }
    }
}

/// Return `true` if any line has at least 2 runs of 3+ whitespace chars
/// between visible tokens AND we exceed `threshold` such lines. Tables
/// flatten into this pattern under text-layer extraction.
pub fn has_columnar_lines(text: &str, threshold: usize) -> bool {
    let mut count = 0usize;
    for raw in text.split('\n') {
        let line = raw.trim();
        if line.len() < 10 {
            continue;
        }
        let mut matches = 0usize;
        let bytes = line.as_bytes();
        let mut i = 0usize;
        while i + 4 < bytes.len() {
            if bytes[i].is_ascii_whitespace() {
                i += 1;
                continue;
            }
            // count run of whitespace after a non-ws byte
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            let ws_len = j - i - 1;
            if ws_len >= 3 && j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                matches += 1;
                if matches >= 2 {
                    break;
                }
            }
            i = j;
        }
        if matches >= 2 {
            count += 1;
            if count > threshold {
                return true;
            }
        }
    }
    false
}

/// Count `$X,XXX(.XX)?`-style currency patterns. Returns `true` when the
/// count exceeds `threshold`.
pub fn has_dense_currency(text: &str, threshold: usize) -> bool {
    // Hand-rolled scan to avoid pulling in `regex` for a single pattern.
    // Pattern: `$` then optional space then one-or-more (digit | ',') then
    // optional `.digits`.
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut count = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        if j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        let digits_start = j;
        while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b',') {
            j += 1;
        }
        if j > digits_start {
            // Optional `.digits`
            if j < bytes.len() && bytes[j] == b'.' {
                let mut k = j + 1;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k > j + 1 {
                    j = k;
                }
            }
            count += 1;
            if count > threshold {
                return true;
            }
            i = j;
            continue;
        }
        i += 1;
    }
    false
}

/// Return `true` when the unpdf text layer is good enough to use directly
/// (i.e. `SmartRouter` should NOT fall back to OCR).
///
/// Port of `isUnpdfSufficient` from TS.
pub fn is_unpdf_sufficient(text: &str, page_count: u32, opts: &RouterOpts) -> bool {
    let pages = page_count.max(1) as usize;
    if text.is_empty() || text.len() < opts.min_chars_per_page * pages {
        return false;
    }
    if has_columnar_lines(text, opts.max_columnar_lines) {
        return false;
    }
    if has_dense_currency(text, opts.max_currency_matches) {
        return false;
    }
    true
}

/// Like [`is_unpdf_sufficient`] but also rejects large PDFs whose extracted
/// text is a tiny fraction of the source file — a strong signal the document
/// is image/design-heavy (e.g. brochures) where the real content lives in
/// pictures the text layer can't see. [`SmartRouter`] uses this since it has
/// the original byte length on hand.
///
/// [`SmartRouter`]: crate::kb::extract::smart_router::SmartRouterExtractor
pub fn is_unpdf_sufficient_with_size(
    text: &str,
    page_count: u32,
    file_bytes: usize,
    opts: &RouterOpts,
) -> bool {
    if !is_unpdf_sufficient(text, page_count, opts) {
        return false;
    }
    // Below this size the text/file ratio is too noisy to trust (small PDFs are
    // mostly structure overhead), so the density guard is skipped.
    const SIZE_FLOOR_BYTES: usize = 1_000_000;
    if file_bytes > SIZE_FLOOR_BYTES {
        let ratio = text.len() as f32 / file_bytes as f32;
        if ratio < opts.min_text_filesize_ratio {
            return false;
        }
    }
    true
}
