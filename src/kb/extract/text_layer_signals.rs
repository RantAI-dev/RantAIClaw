//! Text-layer-sufficiency heuristics shell — real implementation lands in
//! task 5.6.

#[derive(Debug, Clone, Copy)]
pub struct RouterOpts {
    pub min_chars_per_page: usize,
    pub max_columnar_lines: usize,
    pub max_currency_matches: usize,
}

impl Default for RouterOpts {
    fn default() -> Self {
        Self {
            min_chars_per_page: 300,
            max_columnar_lines: 5,
            max_currency_matches: 10,
        }
    }
}

/// Stub: until task 5.6 lands, every text-layer output is rejected so the
/// SmartRouter falls through to its fallback. Conservative.
pub fn is_unpdf_sufficient(_text: &str, _page_count: u32, _opts: &RouterOpts) -> bool {
    false
}
