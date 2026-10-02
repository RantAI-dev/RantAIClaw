//! Words: how a question and a stored note are split for recall.
//!
//! The question and the note go through one splitter, so a word matches only
//! when both sides spell it the same way. Punctuation is a separator, case does
//! not matter, and the common words of a question ("what", "kapan") are dropped
//! from the question side so they cannot be the reason a note scores.

/// Most question words one recall scores.
pub(crate) const MAX_QUERY_TERMS: usize = 8;

/// English function words. An apostrophe stays inside a word, so a contraction
/// is one entry.
const STOPWORDS_EN: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "of", "to", "in", "on", "at", "for", "from", "with",
    "by", "as", "is", "are", "was", "were", "be", "been", "am", "do", "does", "did", "have", "has",
    "had", "i", "me", "my", "you", "your", "we", "our", "they", "it", "its", "this", "that",
    "these", "those", "what", "when", "where", "who", "which", "why", "how", "can", "could",
    "will", "would", "should", "may", "might", "please", "not", "no", "if", "then", "than", "so",
    "about", "into", "after", "before", "during", "between", "while", "any", "some", "there",
    "here", "also", "just", "don't", "it's", "what's", "that's", "isn't", "i'm", "can't", "won't",
];

/// Indonesian function words.
const STOPWORDS_ID: &[&str] = &[
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

fn is_stopword(word: &str) -> bool {
    STOPWORDS_EN.contains(&word) || STOPWORDS_ID.contains(&word)
}

/// Every word of `text`, lowercased and in order, stopwords included.
pub(crate) fn words(text: &str) -> Vec<String> {
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

/// The words of a question that can decide a match: no stopwords, no repeats,
/// at most [`MAX_QUERY_TERMS`].
pub(crate) fn query_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in words(text) {
        if terms.len() == MAX_QUERY_TERMS {
            break;
        }
        if !is_stopword(&word) && !terms.contains(&word) {
            terms.push(word);
        }
    }
    terms
}

/// True when `text` holds at least one word that is not a stopword.
pub(crate) fn has_meaningful_word(text: &str) -> bool {
    !query_terms(text).is_empty()
}

/// The words an explicit search scores. The question's meaningful words, or,
/// when it has none, all of its words: a person who searches for "who" gets
/// the notes that hold it.
pub(crate) fn search_terms(text: &str) -> Vec<String> {
    let terms = query_terms(text);
    if !terms.is_empty() {
        return terms;
    }
    let mut all: Vec<String> = Vec::new();
    for word in words(text) {
        if all.len() == MAX_QUERY_TERMS {
            break;
        }
        if !all.contains(&word) {
            all.push(word);
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_split_on_punctuation_and_ignore_case() {
        assert_eq!(
            words("Release, schedule? (Mobile-App) deploy_window"),
            vec!["release", "schedule", "mobile", "app", "deploy", "window"]
        );
    }

    #[test]
    fn an_apostrophe_inside_a_word_stays_and_one_at_the_edge_does_not() {
        assert_eq!(
            words("don't 'quoted' o\u{2019}clock"),
            vec!["don't", "quoted", "o'clock"]
        );
    }

    #[test]
    fn digits_and_non_ascii_letters_are_words() {
        assert_eq!(words("Rust 1.92 café"), vec!["rust", "1", "92", "café"]);
    }

    #[test]
    fn query_terms_drop_english_and_indonesian_stopwords() {
        assert_eq!(
            query_terms("When is the mobile app release?"),
            vec!["mobile", "app", "release"]
        );
        assert_eq!(
            query_terms("Kapan rilis aplikasi seluler?"),
            vec!["rilis", "aplikasi", "seluler"]
        );
    }

    #[test]
    fn query_terms_keep_each_word_once_and_cap_the_count() {
        assert_eq!(
            query_terms("deploy deploy window"),
            vec!["deploy", "window"]
        );
        let many = "w1 w2 w3 w4 w5 w6 w7 w8 w9 w10";
        assert_eq!(query_terms(many).len(), MAX_QUERY_TERMS);
    }

    #[test]
    fn a_question_of_only_stopwords_has_no_meaningful_word() {
        assert!(!has_meaningful_word("What is that?"));
        assert!(!has_meaningful_word("Apa itu? Siapa dia."));
        assert!(!has_meaningful_word("  ?! "));
        assert!(has_meaningful_word("What is Rust?"));
    }

    #[test]
    fn a_search_of_only_stopwords_still_searches_its_words() {
        assert_eq!(search_terms("who is it"), vec!["who", "is", "it"]);
        assert_eq!(search_terms("who owns the build"), vec!["owns", "build"]);
    }
}
