//! HTML renderers. Telegram supports a small tag set (`<b><i><u><s><code><pre><a>`)
//! and no headings/tables, so headings become `<b>` and tables become a `<pre>`
//! ASCII grid. Matrix (`org.matrix.custom.html`) supports headings and lists;
//! its tables also go to `<pre>` because client `<table>` support is inconsistent.

use super::ast::{Block, Inline};
use super::nest::{indent_continuation, prefix_lines};
use super::split::join_all;
use super::table::ascii_table;
use super::{BlockKind, CodeWrap, RenderedBlock};

/// Telegram's quote marker. Already escaped: it is emitted into HTML.
const TG_QUOTE: &str = "&gt; ";

/// Escape the characters that break HTML parsing. `"` and `'` matter because
/// `escape_html` is also used in the `<a href="…">` attribute context.
pub(crate) fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[derive(Clone, Copy)]
enum Dialect {
    Telegram,
    Matrix,
}

fn wrap_tag(out: &mut String, tag: &str, inner: &str) {
    out.push('<');
    out.push_str(tag);
    out.push('>');
    out.push_str(inner);
    out.push_str("</");
    out.push_str(tag);
    out.push('>');
}

fn inlines_html(inlines: &[Inline], dialect: Dialect) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) => out.push_str(&escape_html(t)),
            Inline::Code(t) => wrap_tag(&mut out, "code", &escape_html(t)),
            Inline::Strong(c) => wrap_tag(&mut out, "b", &inlines_html(c, dialect)),
            Inline::Emphasis(c) => wrap_tag(&mut out, "i", &inlines_html(c, dialect)),
            Inline::Strikethrough(c) => wrap_tag(&mut out, "s", &inlines_html(c, dialect)),
            Inline::Link { text, url } => {
                out.push_str("<a href=\"");
                out.push_str(&escape_html(url));
                out.push_str("\">");
                out.push_str(&inlines_html(text, dialect));
                out.push_str("</a>");
            }
            // Correct for BOTH dialects, for opposite reasons: a soft break means
            // "join with a space", and `\n` collapses to exactly one space in
            // Matrix's real HTML while Telegram's HTML mode keeps it as the
            // newline the author typed.
            Inline::SoftBreak => out.push('\n'),
            // A hard break must survive Matrix's whitespace collapsing, where a
            // bare `\n` renders as a space and the break is silently lost; `<br/>`
            // is in the `org.matrix.custom.html` allowed-tag list precisely for
            // this. Telegram's HTML mode is NOT real HTML — it preserves `\n` and
            // has no `<br>` — so it keeps the newline.
            Inline::HardBreak => match dialect {
                Dialect::Telegram => out.push('\n'),
                Dialect::Matrix => out.push_str("<br/>"),
            },
        }
    }
    out
}

pub fn render_telegram(blocks: &[Block]) -> Vec<RenderedBlock> {
    render(blocks, Dialect::Telegram)
}

pub fn render_matrix(blocks: &[Block]) -> Vec<RenderedBlock> {
    render(blocks, Dialect::Matrix)
}

fn render(blocks: &[Block], dialect: Dialect) -> Vec<RenderedBlock> {
    blocks.iter().map(|b| render_block(b, dialect)).collect()
}

fn render_block(block: &Block, dialect: Dialect) -> RenderedBlock {
    match block {
        Block::Heading { level, inlines } => {
            let inner = inlines_html(inlines, dialect);
            let mut text = String::new();
            match dialect {
                Dialect::Telegram => wrap_tag(&mut text, "b", &inner),
                Dialect::Matrix => {
                    let tag = format!("h{level}");
                    wrap_tag(&mut text, &tag, &inner);
                }
            }
            RenderedBlock::prose_html(text)
        }
        Block::Paragraph(inlines) => {
            let inner = inlines_html(inlines, dialect);
            // One `RenderedBlock` either way — the renderer invariant `split_paired`
            // relies on holds: `<p>` wraps, it does not split the block in two.
            RenderedBlock::prose_html(match dialect {
                // Telegram's HTML mode preserves `\n`, so the `"\n\n"` that
                // `split` joins blocks with already separates paragraphs, and
                // `<p>` is not in its tag set.
                Dialect::Telegram => inner,
                // Matrix `formatted_body` IS real HTML: whitespace collapses, so
                // that same `"\n\n"` renders as a single space and every
                // paragraph boundary is lost. `<p>` is what carries it.
                Dialect::Matrix => {
                    let mut text = String::new();
                    wrap_tag(&mut text, "p", &inner);
                    text
                }
            })
        }
        Block::CodeBlock { code, .. } => {
            RenderedBlock::code(escape_html(code.trim_end_matches('\n')), CodeWrap::HtmlPre)
        }
        Block::List {
            ordered,
            start,
            items,
        } => RenderedBlock::prose_html(list_html(*ordered, *start, items, dialect)),
        Block::BlockQuote(inner) => {
            let rendered = render(inner, dialect);
            let mut text = String::new();
            match dialect {
                Dialect::Telegram => {
                    for (i, sub) in rendered.iter().enumerate() {
                        if i > 0 {
                            // The blank line `join_all` would have put between two
                            // blocks, quoted — `prefix_lines` renders a blank line
                            // as the trimmed marker, so this matches it exactly.
                            text.push('\n');
                            text.push_str(TG_QUOTE.trim_end());
                            text.push('\n');
                        }
                        // `join_all`, NOT `.text`: a `Code` sub-block holds the RAW
                        // body with its `<pre>` deferred to `code_wrap`.
                        let piece = join_all(std::slice::from_ref(sub));
                        if sub.kind == BlockKind::Code {
                            // ATOMIC — see `list_html`. Quoting a `<pre>`'s interior
                            // puts literal `&gt;` markers inside the code.
                            text.push_str(TG_QUOTE);
                            text.push_str(&piece);
                        } else {
                            text.push_str(&prefix_lines(&piece, TG_QUOTE));
                        }
                    }
                }
                // Matrix nests real elements instead of marking lines, so no
                // `<pre>` body is ever traversed line-wise here.
                Dialect::Matrix => wrap_tag(&mut text, "blockquote", &join_all(&rendered)),
            }
            RenderedBlock::prose_html(text)
        }
        Block::Table { headers, rows, .. } => {
            RenderedBlock::code(escape_html(&ascii_table(headers, rows)), CodeWrap::HtmlPre)
        }
        Block::Rule => RenderedBlock::prose_html(match dialect {
            Dialect::Telegram => "──────────".to_string(),
            Dialect::Matrix => "<hr>".to_string(),
        }),
    }
}

fn list_html(ordered: bool, start: u64, items: &[Vec<Block>], dialect: Dialect) -> String {
    match dialect {
        Dialect::Matrix => {
            let tag = if ordered { "ol" } else { "ul" };
            let mut body = String::new();
            for item in items {
                // A lone paragraph is CommonMark's TIGHT item, which renders
                // without `<p>`. The AST cannot report tightness — it synthesizes
                // a `Paragraph` around a tight item's bare text — so match the
                // shape instead: `- a` stays `<li>a</li>`, while a genuinely
                // multi-block item keeps the `<p>` boundaries it would otherwise
                // lose. Wrapping unconditionally would render EVERY Matrix list
                // loose (extra vertical space on the commonest list there is).
                let inner = match item.as_slice() {
                    [Block::Paragraph(inlines)] => inlines_html(inlines, dialect),
                    blocks => join_all(&render(blocks, dialect)),
                };
                wrap_tag(&mut body, "li", &inner);
            }
            // Not `wrap_tag`: it reuses one string for the open AND close tag, so
            // an attribute would emit the malformed `</ol start="3">`. `start` must
            // survive — without it a client renumbers `3. a` as "1.", and the AST
            // carries `start` precisely so it does not (the Telegram arm honors it).
            let mut out = String::new();
            out.push('<');
            out.push_str(tag);
            if ordered && start != 1 {
                out.push_str(" start=\"");
                out.push_str(&start.to_string());
                out.push('"');
            }
            out.push('>');
            out.push_str(&body);
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
            out
        }
        Dialect::Telegram => {
            let mut parts = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let mut marker = if ordered {
                    let n = usize::try_from(start).unwrap_or(1) + i;
                    let mut m = n.to_string();
                    m.push_str(". ");
                    m
                } else {
                    "• ".to_string()
                };
                let width = marker.chars().count();
                let indent = " ".repeat(width);
                let rendered = render(item, dialect);
                let mut body = String::new();
                for (j, sub) in rendered.iter().enumerate() {
                    if j > 0 {
                        // The blank line `join_all` puts between two blocks;
                        // `indent_continuation` leaves blank lines blank, so this
                        // matches what indenting the joined body produced.
                        body.push_str("\n\n");
                    }
                    // `join_all`, NOT `.text`: a `Code` sub-block holds the RAW body
                    // with its fence deferred to `code_wrap`, so `.text` would drop
                    // the `<pre>` and inline the snippet into the bullet.
                    let piece = join_all(std::slice::from_ref(sub));
                    match (sub.kind, j) {
                        // A materialized `<pre>` is ATOMIC. `indent_continuation`
                        // and `prefix_lines` are line-oriented and cannot see that
                        // `<pre>` is open: the open tag sits on the piece's FIRST
                        // line, so line 1's content escaped the indent while lines
                        // 2+ took it INSIDE the element — silently rewriting the
                        // model's code and knocking an ASCII table out of the
                        // alignment that is its whole point. Only the line the
                        // `<pre>` opens on is indented (that indent is outside the
                        // element), so the body stays byte-identical to the source.
                        (BlockKind::Code, 0) => body.push_str(&piece),
                        (BlockKind::Code, _) => {
                            body.push_str(&indent);
                            body.push_str(&piece);
                        }
                        // The marker occupies the first line's indent column.
                        (_, 0) => body.push_str(&indent_continuation(&piece, width)),
                        _ => body.push_str(&prefix_lines(&piece, &indent)),
                    }
                }
                marker.push_str(body.trim_end());
                parts.push(marker.trim_end().to_string());
            }
            parts.join("\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::format::ast::parse;

    fn tg(md: &str) -> String {
        render_telegram(&parse(md))
            .iter()
            .map(|b| b.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
    fn mx(md: &str) -> String {
        render_matrix(&parse(md))
            .iter()
            .map(|b| b.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    #[test]
    fn escapes_angle_brackets_and_amp() {
        assert_eq!(escape_html("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }

    #[test]
    fn escapes_quotes_for_attribute_context() {
        assert_eq!(escape_html("a\"b'c"), "a&quot;b&#39;c");
    }

    #[test]
    fn telegram_bold_uses_b_tag() {
        assert_eq!(tg("**hi**"), "<b>hi</b>");
    }

    #[test]
    fn telegram_heading_becomes_bold() {
        assert_eq!(tg("## Title"), "<b>Title</b>");
    }

    #[test]
    fn telegram_script_is_escaped_not_injected() {
        let out = tg("<script>x</script>");
        assert!(out.contains("&lt;script&gt;"), "{out}");
        // The negative is the half that matters: the positive alone passes even
        // if a raw `<script>` rides along beside the escaped one.
        assert!(!out.contains("<script>"), "raw markup survived: {out}");
    }

    #[test]
    fn link_url_cannot_break_out_of_href() {
        let out = tg("[x](https://a\"onmouseover=\"evil)");
        // Pin the attribute context FIRST. If pulldown ever stopped parsing that
        // destination as a link, the text would be escaped as ordinary prose and
        // both asserts below would still pass — the guard would silently stop
        // covering the thing it is named for.
        assert!(out.contains("<a href="), "not an attribute context: {out}");
        assert!(!out.contains("\"onmouseover=\""), "{out}");
        assert!(out.contains("&quot;"), "{out}");
    }

    #[test]
    fn matrix_heading_uses_hn() {
        assert_eq!(mx("## Title"), "<h2>Title</h2>");
    }

    // Without `start`, a client renumbers "3." as "1." — the AST carries it and
    // the Telegram arm honors it, so Matrix must too.
    #[test]
    fn matrix_ordered_list_keeps_start() {
        assert_eq!(
            mx("3. a\n4. b"),
            "<ol start=\"3\"><li>a</li><li>b</li></ol>"
        );
    }

    #[test]
    fn matrix_ordered_list_from_one_omits_start() {
        assert_eq!(mx("1. a"), "<ol><li>a</li></ol>");
    }

    #[test]
    fn matrix_unordered_list_has_no_start() {
        assert_eq!(mx("- a"), "<ul><li>a</li></ul>");
    }

    #[test]
    fn telegram_link_uses_anchor() {
        assert_eq!(
            tg("[docs](https://x.io)"),
            r#"<a href="https://x.io">docs</a>"#
        );
    }

    // Every block kind, not just the three that happen to be one-liners. The
    // invariant is structural (`render` is a `.map()`), so no input can falsify
    // it today — but a renderer that grew a second block would grow it in ONE
    // arm (a table caption, a rule's spacer, a `<pre>`'s language label) behind
    // a `flat_map`, and only that arm's own input can catch it.
    #[test]
    fn one_rendered_block_per_input_block() {
        let blocks = parse("# a\n\np\n\n- x\n\n```\nc\n```\n\n> q\n\n| A |\n|---|\n| 1 |\n\n---");
        // Guards the assertion itself: `len() == len()` is vacuously true if the
        // input collapsed to fewer kinds than this test means to cover.
        assert_eq!(blocks.len(), 7, "input lost a block: {blocks:?}");
        assert_eq!(render_telegram(&blocks).len(), blocks.len());
        assert_eq!(render_matrix(&blocks).len(), blocks.len());
    }

    #[test]
    fn telegram_code_in_list_item_keeps_pre() {
        let out = tg("1. Run:\n\n   ```\n   cmd\n   two\n   ```");
        assert!(out.contains("<pre>cmd\ntwo</pre>"), "pre dropped: {out}");
    }

    #[test]
    fn matrix_code_in_list_item_keeps_pre() {
        let out = mx("1. Run:\n\n   ```\n   cmd\n   two\n   ```");
        assert!(out.contains("<pre>cmd\ntwo</pre>"), "pre dropped: {out}");
    }

    /// Every `<pre>…</pre>` body in `s`, so a test can assert the code the model
    /// wrote survived byte-for-byte.
    fn pre_bodies(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(start) = rest.find("<pre>") {
            let after = &rest[start + "<pre>".len()..];
            let Some(end) = after.find("</pre>") else {
                break;
            };
            out.push(after[..end].to_string());
            rest = &after[end..];
        }
        out
    }

    /// Final element depth: 0 means every tag opened was also closed.
    ///
    /// A DELIBERATE reimplementation of `split::hard_split_html`'s walk, not a
    /// call into it — do not "DRY" this away. This oracle checks that
    /// function's OUTPUT, so classifying tags with the classifier under test
    /// would make it blind to the bug it is here to catch: a tag `split`
    /// wrongly thinks is VOID gets flushed as if the element were complete, and
    /// the two halves can land in different chunks. Sharing the classifier, both
    /// sides would call the severed chunk balanced and agree it was fine.
    ///
    /// So it is written to be able to DISAGREE:
    /// - `split` PREFIX-matches `<br`/`<hr`, a deliberate over-approximation (it
    ///   documents why: guessing void is the safe direction there). This
    ///   exact-matches the `<hr>` and `…/>` forms the renderers above actually
    ///   emit, so if that prefix ever over-reaches onto a real element, the
    ///   severed chunk fails here instead of passing on both sides.
    /// - `split` clamps with `.max(0)` because it must keep making progress on
    ///   malformed input; this lets depth go NEGATIVE, so a surplus close tag is
    ///   a failure rather than a silent floor.
    ///
    /// (Proven: making `split` treat `<b>` as void severs `"&gt; quoted <b>"`
    /// from its `</b>` and this fails with depth 1.)
    fn tag_depth(s: &str) -> i32 {
        let mut depth = 0;
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '<' {
                continue;
            }
            let closing = chars.peek() == Some(&'/');
            let mut tag = String::from('<');
            for c in chars.by_ref() {
                tag.push(c);
                if c == '>' {
                    break;
                }
            }
            let void = tag.ends_with("/>") || tag == "<hr>";
            if closing {
                depth -= 1;
            } else if !void {
                depth += 1;
            }
        }
        depth
    }

    // A ONE-line body cannot discriminate this: `<pre>` opens on the body's FIRST
    // line, and only a CONTINUATION line can pick up the item's indent. Two lines
    // is the shortest input that can fail — with the indent applied to the
    // materialized string, this body came out `"aaaa\n   bb"`.
    #[test]
    fn telegram_list_item_never_indents_inside_a_pre() {
        let out = tg("1. Run:\n\n   ```\n   aaaa\n   bb\n   ```");
        assert_eq!(pre_bodies(&out), vec!["aaaa\nbb"], "{out}");
    }

    // Same blindness, worse payload: line-quoting a materialized `<pre>` put
    // literal `&gt;` markers INSIDE the code (`"a\n&gt;\n&gt; b"`).
    #[test]
    fn telegram_blockquote_never_quotes_inside_a_pre() {
        let out = tg("> ```\n> a\n> \n> b\n> ```");
        assert_eq!(pre_bodies(&out), vec!["a\n\nb"], "{out}");
        // The quote marker still opens the block — only the interior is spared.
        assert!(out.starts_with("&gt; <pre>"), "quote marker lost: {out}");
    }

    // An ASCII table's whole purpose is alignment. Indenting lines 2+ but not
    // line 1 (which rides the `<pre>` tag) skewed exactly the first row.
    #[test]
    fn telegram_table_in_list_item_stays_aligned() {
        let out = tg(
            "1. Table:\n\n   | Step | Perintah |\n   |---|---|\n   | 1 | python3 --version |\n   | 2 | mkdir x |",
        );
        let bodies = pre_bodies(&out);
        assert_eq!(bodies.len(), 1, "table did not reach a <pre>: {out}");
        let widths: Vec<usize> = bodies[0].lines().map(|l| l.chars().count()).collect();
        // Guards the assertion itself: a table that silently parsed as prose
        // would leave too few lines for the width check to mean anything.
        assert_eq!(widths.len(), 4, "not a grid: {out}");
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "ragged {widths:?}: {out}"
        );
    }

    // Matrix `formatted_body` is real HTML: the `"\n\n"` between blocks collapses,
    // so without `<p>` these two paragraphs render as one line.
    #[test]
    fn matrix_paragraphs_become_p_elements() {
        assert_eq!(
            mx("Para one.\n\nPara two."),
            "<p>Para one.</p>\n\n<p>Para two.</p>"
        );
    }

    #[test]
    fn matrix_hard_break_becomes_br() {
        assert_eq!(mx("line a  \nline b"), "<p>line a<br/>line b</p>");
    }

    // A soft break means "join with a space": `\n` collapses to exactly that in
    // real HTML, so it must NOT become `<br/>`.
    #[test]
    fn matrix_soft_break_stays_a_newline() {
        assert_eq!(mx("line a\nline b"), "<p>line a\nline b</p>");
    }

    // Telegram's HTML mode is not real HTML — `\n` IS the break and `<p>`/`<br>`
    // are not in its tag set. The Defect-2 fix must not leak into it.
    #[test]
    fn telegram_paragraphs_and_breaks_use_newlines_not_tags() {
        assert_eq!(tg("Para one.\n\nPara two."), "Para one.\n\nPara two.");
        assert_eq!(tg("line a  \nline b"), "line a\nline b");
    }

    // CommonMark renders a TIGHT item without `<p>`. The AST cannot report
    // tightness (it synthesizes a `Paragraph` around a tight item's bare text),
    // so wrapping unconditionally would render every Matrix list loose.
    #[test]
    fn matrix_tight_list_item_keeps_no_paragraph() {
        assert_eq!(mx("- a\n- b"), "<ul><li>a</li><li>b</li></ul>");
    }

    // …but `<li>` only separates ITEMS, not the blocks inside one: without `<p>`
    // these two paragraphs collapse into "Para one. Para two.".
    #[test]
    fn matrix_multi_block_list_item_keeps_its_paragraph_boundary() {
        assert_eq!(
            mx("1. Para one.\n\n   Para two."),
            "<ol><li><p>Para one.</p>\n\n<p>Para two.</p></li></ol>"
        );
    }

    #[test]
    fn split_keeps_tags_balanced_across_nested_and_paragraph_shapes() {
        let cases = [
            "1. Run:\n\n   ```\n   aaaa\n   bb\n   ```",
            "> ```\n> a\n> \n> b\n> ```",
            "1. Table:\n\n   | Step | Perintah |\n   |---|---|\n   | 1 | python3 --version |\n   | 2 | mkdir x |",
            "Para one.\n\nPara two.",
            "line a  \nline b",
            "1. Para one.\n\n   Para two.",
            "> quoted **bold**\n>\n> second",
            // The two void tags the renderers emit — `<hr>` (Matrix rule) and
            // `<br/>` (hard break, above). Without a rule in the corpus, the
            // void arm of `tag_depth` and of `hard_split_html` is never taken.
            "a\n\n---\n\nb",
        ];
        for md in cases {
            for limit in [16, 64, 4096] {
                for blocks in [render_telegram(&parse(md)), render_matrix(&parse(md))] {
                    for chunk in super::super::split::split(&blocks, limit) {
                        assert_eq!(tag_depth(&chunk), 0, "unbalanced {chunk:?} from {md:?}");
                    }
                }
            }
        }
    }
}
