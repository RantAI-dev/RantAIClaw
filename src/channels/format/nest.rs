//! Layout helpers for blocks nested inside a list item or a blockquote.
//!
//! A nested renderer must NOT read [`RenderedBlock::text`] directly: a `Code`
//! block holds the RAW body with its fence/`<pre>`/indent deferred to
//! `code_wrap`, so reading `.text` drops the wrapper and a fenced block inside a
//! list item renders as bare prose. Use [`super::split::join_all`], which
//! materializes every sub-block's wrapper, then lay the result out with these.

/// Prefix every line of `s`. A blank line gets the trimmed prefix, so quoting
/// never emits trailing whitespace.
pub(crate) fn prefix_lines(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| {
            if l.is_empty() {
                prefix.trim_end().to_string()
            } else {
                let mut out = String::from(prefix);
                out.push_str(l);
                out
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Indent every line after the first by `width` spaces, so a list item's
/// continuation blocks sit in the item's content column. The first line stays
/// bare (the marker goes there) and blank lines stay blank.
pub(crate) fn indent_continuation(s: &str, width: usize) -> String {
    let indent = " ".repeat(width);
    let mut out = String::new();
    for (i, l) in s.lines().enumerate() {
        if i > 0 {
            out.push('\n');
            if !l.is_empty() {
                out.push_str(&indent);
            }
        }
        out.push_str(l);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_lines_marks_every_line() {
        assert_eq!(prefix_lines("a\nb", "> "), "> a\n> b");
    }

    #[test]
    fn prefix_lines_leaves_no_trailing_space_on_a_blank_line() {
        assert_eq!(prefix_lines("a\n\nb", "> "), "> a\n>\n> b");
    }

    #[test]
    fn indent_continuation_leaves_the_first_line_bare() {
        assert_eq!(indent_continuation("a\nb", 2), "a\n  b");
    }

    #[test]
    fn indent_continuation_keeps_blank_lines_blank() {
        assert_eq!(indent_continuation("a\n\nb", 3), "a\n\n   b");
    }

    #[test]
    fn indent_continuation_of_empty_is_empty() {
        assert_eq!(indent_continuation("", 2), "");
    }
}
