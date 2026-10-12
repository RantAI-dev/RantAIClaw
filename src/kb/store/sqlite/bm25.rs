//! BM25 lexical search via SQLite's FTS5 virtual table.
//!
//! SQLite's `bm25()` returns a *lower-is-better* score (smaller distance from
//! the query). The TS side (`search::score(0)` convention) treats higher as
//! better and downstream RRF/rerank code assumes the same. Negate before
//! returning so the contract matches across backends.
//!
//! The query is never bound to `MATCH` as typed. FTS5 parses its own query
//! syntax, so a `?`, `@`, `:`, a bare `AND` or a stray `"` would fail the whole
//! search. The text is reduced to its words first, and each word is a quoted
//! FTS5 string, so no caller character reaches the parser as syntax.

use rusqlite::params;

use super::SqliteStore;
use crate::kb::store::Bm25Hit;
use crate::kb::{ChunkId, DocumentId, KbError, KbResult};

/// Most query words one keyword search scores. Same cap as the memory
/// recall builder (`MAX_QUERY_TERMS` in `src/memory/terms.rs`).
const MAX_QUERY_TERMS: usize = 8;

/// English and Indonesian function words that cannot decide a match. These
/// are the lists in `src/memory/terms.rs`, copied rather than imported: a
/// shared helper across `memory`, `sessions` and `kb` is a later refactor, and
/// `kb` must not depend on the memory module. Keep the two lists in step.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "and",
    "or",
    "but",
    "of",
    "to",
    "in",
    "on",
    "at",
    "for",
    "from",
    "with",
    "by",
    "as",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "am",
    "do",
    "does",
    "did",
    "have",
    "has",
    "had",
    "i",
    "me",
    "my",
    "you",
    "your",
    "we",
    "our",
    "they",
    "it",
    "its",
    "this",
    "that",
    "these",
    "those",
    "what",
    "when",
    "where",
    "who",
    "which",
    "why",
    "how",
    "can",
    "could",
    "will",
    "would",
    "should",
    "may",
    "might",
    "please",
    "not",
    "no",
    "if",
    "then",
    "than",
    "so",
    "about",
    "into",
    "after",
    "before",
    "during",
    "between",
    "while",
    "any",
    "some",
    "there",
    "here",
    "also",
    "just",
    "don't",
    "it's",
    "what's",
    "that's",
    "isn't",
    "i'm",
    "can't",
    "won't",
    "yang",
    "dan",
    "di",
    "ke",
    "dari",
    "untuk",
    "dengan",
    "ini",
    "itu",
    "apa",
    "apakah",
    "siapa",
    "kapan",
    "dimana",
    "mana",
    "bagaimana",
    "kenapa",
    "mengapa",
    "berapa",
    "adalah",
    "ada",
    "akan",
    "atau",
    "pada",
    "dalam",
    "saya",
    "aku",
    "kamu",
    "anda",
    "kita",
    "kami",
    "dia",
    "mereka",
    "tidak",
    "bukan",
    "sudah",
    "belum",
    "bisa",
    "dapat",
    "tolong",
    "mohon",
    "ya",
    "setelah",
    "sebelum",
    "saat",
    "ketika",
    "jika",
    "kalau",
    "juga",
];

/// Every word of `text`, lowercased and in order. A word is a run of Unicode
/// letters and digits. An apostrophe between two such characters stays inside
/// the word (`it's`), with either the ASCII or the typographic one folded to
/// `'`. Everything else separates words.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if matches!(ch, '\'' | '\u{2019}')
            && !current.is_empty()
            && chars.peek().is_some_and(|next| next.is_alphanumeric())
        {
            current.push('\'');
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// The FTS5 MATCH expression for a free-text query: each meaningful word as a
/// double-quoted string, joined with ` OR `. Returns `None` when the text holds
/// no meaningful word (punctuation only, or stopwords only), so the caller can
/// answer without touching SQLite.
///
/// Words are joined with OR where the raw text was an implicit AND. A plain
/// question then matches chunks that hold some of its words, ranked by BM25.
///
/// Unlike the memory builder, there is no fallback to all words when every word
/// is a stopword: a query of `it's` or `NOT` must return no keyword hits, not
/// search for those words.
fn build_match_expression(query: &str) -> Option<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in words(query) {
        if terms.len() == MAX_QUERY_TERMS {
            break;
        }
        if !STOPWORDS.contains(&word.as_str()) && !terms.contains(&word) {
            terms.push(word);
        }
    }
    if terms.is_empty() {
        return None;
    }
    Some(
        terms
            .iter()
            .map(|w| format!("\"{w}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

impl SqliteStore {
    /// Keyword search over the chunks of live documents.
    ///
    /// # Errors
    ///
    /// Returns a [`KbError`] when the SQLite query fails or the blocking task
    /// cannot be joined. A query with no meaningful word is not an error: it
    /// returns an empty list without running the query.
    pub(crate) async fn bm25_search_impl(
        &self,
        query: &str,
        limit: usize,
    ) -> KbResult<Vec<Bm25Hit>> {
        let Some(expression) = build_match_expression(query) else {
            return Ok(Vec::new());
        };
        let conn = self.conn.clone();
        let limit_i = limit as i64;

        tokio::task::spawn_blocking(move || -> KbResult<Vec<Bm25Hit>> {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT c.id, c.document_id, c.content, bm25(chunk_fts) AS score
                 FROM chunk_fts
                 JOIN chunk c ON c.rowid = chunk_fts.rowid
                 JOIN document d ON d.id = c.document_id
                 WHERE chunk_fts MATCH ?1 AND d.deleted_at IS NULL
                 ORDER BY score
                 LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![expression, limit_i])?;

            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let raw_score: f32 = row.get("score")?;
                out.push(Bm25Hit {
                    id: ChunkId(row.get("id")?),
                    document_id: DocumentId(row.get("document_id")?),
                    content: row.get("content")?,
                    // Negate: SQLite's `bm25()` is lower-is-better, but the
                    // TS retrieval pipeline treats higher as better. A pure
                    // negation preserves rank order and matches downstream
                    // RRF/rerank assumptions.
                    score: -raw_score,
                });
            }
            Ok(out)
        })
        .await
        .map_err(|e| KbError::Other(format!("join: {e}")))?
    }
}
