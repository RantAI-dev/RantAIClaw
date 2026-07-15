//! HTML renderers. Telegram supports a small tag set (`<b><i><u><s><code><pre><a>`)
//! and no headings/tables, so headings become `<b>` and tables become a `<pre>`
//! ASCII grid. Matrix (`org.matrix.custom.html`) supports headings and lists;
//! its tables also go to `<pre>` because client `<table>` support is inconsistent.

use super::ast::{Block, Inline};
use super::table::ascii_table;
use super::{CodeWrap, RenderedBlock};

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

fn inlines_html(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) => out.push_str(&escape_html(t)),
            Inline::Code(t) => wrap_tag(&mut out, "code", &escape_html(t)),
            Inline::Strong(c) => wrap_tag(&mut out, "b", &inlines_html(c)),
            Inline::Emphasis(c) => wrap_tag(&mut out, "i", &inlines_html(c)),
            Inline::Strikethrough(c) => wrap_tag(&mut out, "s", &inlines_html(c)),
            Inline::Link { text, url } => {
                out.push_str("<a href=\"");
                out.push_str(&escape_html(url));
                out.push_str("\">");
                out.push_str(&inlines_html(text));
                out.push_str("</a>");
            }
            Inline::SoftBreak | Inline::HardBreak => out.push('\n'),
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
            let inner = inlines_html(inlines);
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
        Block::Paragraph(inlines) => RenderedBlock::prose_html(inlines_html(inlines)),
        Block::CodeBlock { code, .. } => {
            RenderedBlock::code(escape_html(code.trim_end_matches('\n')), CodeWrap::HtmlPre)
        }
        Block::List {
            ordered,
            start,
            items,
        } => RenderedBlock::prose_html(list_html(*ordered, *start, items, dialect)),
        Block::BlockQuote(inner) => {
            let body = render(inner, dialect)
                .iter()
                .map(|b| b.text.clone())
                .collect::<Vec<_>>()
                .join("\n");
            let mut text = String::new();
            match dialect {
                Dialect::Telegram => {
                    text = body
                        .lines()
                        .map(|l| {
                            let mut s = String::from("&gt; ");
                            s.push_str(l);
                            s
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                }
                Dialect::Matrix => wrap_tag(&mut text, "blockquote", &body),
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
                let inner = render(item, dialect)
                    .iter()
                    .map(|b| b.text.clone())
                    .collect::<Vec<_>>()
                    .join(" ");
                wrap_tag(&mut body, "li", &inner);
            }
            let mut out = String::new();
            wrap_tag(&mut out, tag, &body);
            out
        }
        Dialect::Telegram => {
            let mut text = String::new();
            for (i, item) in items.iter().enumerate() {
                if ordered {
                    let n = usize::try_from(start).unwrap_or(1) + i;
                    text.push_str(&n.to_string());
                    text.push_str(". ");
                } else {
                    text.push_str("• ");
                }
                let inner = render(item, dialect)
                    .iter()
                    .map(|b| b.text.clone())
                    .collect::<Vec<_>>()
                    .join(" ");
                text.push_str(inner.trim());
                text.push('\n');
            }
            text.trim_end().to_string()
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
        assert!(tg("<script>x</script>").contains("&lt;script&gt;"));
    }

    #[test]
    fn link_url_cannot_break_out_of_href() {
        let out = tg("[x](https://a\"onmouseover=\"evil)");
        assert!(!out.contains("\"onmouseover=\""));
        assert!(out.contains("&quot;"));
    }

    #[test]
    fn matrix_heading_uses_hn() {
        assert_eq!(mx("## Title"), "<h2>Title</h2>");
    }

    #[test]
    fn telegram_link_uses_anchor() {
        assert_eq!(
            tg("[docs](https://x.io)"),
            r#"<a href="https://x.io">docs</a>"#
        );
    }

    #[test]
    fn one_rendered_block_per_input_block() {
        let blocks = parse("# a\n\np\n\n- x");
        assert_eq!(render_telegram(&blocks).len(), blocks.len());
        assert_eq!(render_matrix(&blocks).len(), blocks.len());
    }
}
