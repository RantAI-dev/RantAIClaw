//! Layout helpers for blocks nested inside a list item or a blockquote.
//!
//! A nested renderer must NOT read [`RenderedBlock::text`] directly: a `Code`
//! block holds the RAW body with its fence/`<pre>`/indent deferred to
//! `code_wrap`, so reading `.text` drops the wrapper and a fenced block inside a
//! list item renders as bare prose. Use [`super::split::join_all`], which
//! materializes every sub-block's wrapper, then lay the result out with these.

/// Prefix every line of `s`. A blank line gets the trimmed prefix, so quoting
/// never emits trailing whitespace.
///
/// "Blank" means whitespace-only, not just empty. `CodeWrap::Indent` renders a
/// code body's blank line as its four-space indent, so a quoted code block
/// arrives here with whitespace-only lines and an `is_empty` test walks straight
/// past them: `> ```/> a/>/> b/> ``` ` quoted back as `">     a\n>     \n>     b"`.
/// Those four spaces are the wrap's own decoration — the source line was empty —
/// so dropping them costs no content. A line with any non-whitespace char keeps
/// every space it has, indentation included.
pub(crate) fn prefix_lines(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| {
            if l.trim().is_empty() {
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
///
/// "Blank" is whitespace-only here for the same reason as [`prefix_lines`]: an
/// indented code block's blank line reaches this as four spaces, and indenting
/// THAT emits a line of nothing but whitespace (a list holding one produced
/// `"1. Run:\n\n       a\n       \n       b"`). Such a line is dropped to empty
/// rather than indented, which is what "blank lines stay blank" always meant.
pub(crate) fn indent_continuation(s: &str, width: usize) -> String {
    let indent = " ".repeat(width);
    let mut out = String::new();
    for (i, l) in s.lines().enumerate() {
        if i > 0 {
            out.push('\n');
            if l.trim().is_empty() {
                // No indent AND no line: indenting whitespace-only content just
                // builds a longer whitespace-only line.
                continue;
            }
            out.push_str(&indent);
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

    // The shape `CodeWrap::Indent` actually hands over: a code body's blank line
    // is rendered as the four-space indent, so the "blank" line is whitespace,
    // not empty. `"a\n\nb"` above cannot catch this — it has a truly empty line,
    // which the `is_empty` test this replaced already handled.
    #[test]
    fn prefix_lines_leaves_no_trailing_space_on_a_whitespace_only_line() {
        assert_eq!(
            prefix_lines("    a\n    \n    b", "> "),
            ">     a\n>\n>     b"
        );
    }

    // A whitespace-only line loses only the wrap's own decoration. Real content
    // keeps every space it has — indentation is what makes code readable.
    #[test]
    fn prefix_lines_keeps_indentation_on_a_line_with_content() {
        assert_eq!(prefix_lines("        deep", "> "), ">         deep");
    }

    #[test]
    fn indent_continuation_leaves_the_first_line_bare() {
        assert_eq!(indent_continuation("a\nb", 2), "a\n  b");
    }

    #[test]
    fn indent_continuation_keeps_blank_lines_blank() {
        assert_eq!(indent_continuation("a\n\nb", 3), "a\n\n   b");
    }

    // As above: an indented code block's blank line arrives as whitespace, and
    // indenting it would emit `width + 4` spaces and nothing else.
    #[test]
    fn indent_continuation_leaves_no_trailing_space_on_a_whitespace_only_line() {
        assert_eq!(
            indent_continuation("Run:\n\n    a\n    \n    b", 3),
            "Run:\n\n       a\n\n       b"
        );
    }

    #[test]
    fn indent_continuation_of_empty_is_empty() {
        assert_eq!(indent_continuation("", 2), "");
    }
}
