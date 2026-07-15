//! LightMarkup renderer: WhatsApp/Slack-style single-char markup — `*bold*`,
//! `_italic_`, `~strike~`, `` `code` ``. Headings become bold. Tables become an
//! ASCII grid inside a fenced block. Links depend on `LinkStyle`.

use super::ast::{Block, Inline};
use super::nest::{indent_continuation, prefix_lines};
use super::split::join_all;
use super::table::ascii_table;
use super::{CodeWrap, LinkStyle, RenderedBlock};

fn escape_slack(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn maybe_escape(s: &str, links: LinkStyle) -> String {
    match links {
        LinkStyle::Slack => escape_slack(s),
        LinkStyle::Raw => s.to_string(),
    }
}

fn wrap_char(out: &mut String, c: char, inner: &str) {
    out.push(c);
    out.push_str(inner);
    out.push(c);
}

fn inlines_light(inlines: &[Inline], links: LinkStyle) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) => out.push_str(&maybe_escape(t, links)),
            Inline::Code(t) => wrap_char(&mut out, '`', &maybe_escape(t, links)),
            Inline::Strong(c) => wrap_char(&mut out, '*', &inlines_light(c, links)),
            Inline::Emphasis(c) => wrap_char(&mut out, '_', &inlines_light(c, links)),
            Inline::Strikethrough(c) => wrap_char(&mut out, '~', &inlines_light(c, links)),
            Inline::Link { text, url } => {
                let label = inlines_light(text, links);
                match links {
                    LinkStyle::Slack => {
                        out.push('<');
                        out.push_str(&escape_slack(url));
                        out.push('|');
                        out.push_str(&label);
                        out.push('>');
                    }
                    LinkStyle::Raw => {
                        out.push_str(&label);
                        out.push_str(" (");
                        out.push_str(url);
                        out.push(')');
                    }
                }
            }
            Inline::SoftBreak | Inline::HardBreak => out.push('\n'),
        }
    }
    out
}

pub fn render(blocks: &[Block], links: LinkStyle) -> Vec<RenderedBlock> {
    blocks.iter().map(|b| render_block(b, links)).collect()
}

fn render_block(block: &Block, links: LinkStyle) -> RenderedBlock {
    match block {
        Block::Heading { inlines, .. } => {
            let mut text = String::new();
            wrap_char(&mut text, '*', &inlines_light(inlines, links));
            RenderedBlock::prose(text)
        }
        Block::Paragraph(inlines) => RenderedBlock::prose(inlines_light(inlines, links)),
        Block::CodeBlock { code, .. } => {
            RenderedBlock::code(code.trim_end_matches('\n'), CodeWrap::Fence(None))
        }
        // Same shape as `plain.rs`/`markdown.rs` (see Task 7b). The loop is
        // duplicated per renderer on purpose: each `render()` takes a different
        // extra argument (`links` here, `tables_native`/`dialect`/none elsewhere),
        // so extracting it would need a closure or generic and would couple the
        // renderers to each other against CLAUDE.md §6.4. CLAUDE.md §3.3 —
        // duplicate small, local logic when it preserves clarity.
        Block::List {
            ordered,
            start,
            items,
        } => {
            let mut parts = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let mut marker = if *ordered {
                    let n = usize::try_from(*start).unwrap_or(1) + i;
                    let mut m = n.to_string();
                    m.push_str(". ");
                    m
                } else {
                    "• ".to_string()
                };
                // `join_all`, NOT `.text`: a `Code` sub-block holds the RAW body
                // with its wrapper deferred to `code_wrap`, so `.text` would drop
                // the fence and inline the snippet into the bullet.
                let body = join_all(&render(item, links));
                let width = marker.chars().count();
                marker.push_str(&indent_continuation(body.trim_end(), width));
                parts.push(marker.trim_end().to_string());
            }
            RenderedBlock::prose(parts.join("\n"))
        }
        Block::BlockQuote(inner) => {
            let body = join_all(&render(inner, links));
            RenderedBlock::prose(prefix_lines(&body, "> "))
        }
        Block::Table { headers, rows, .. } => {
            RenderedBlock::code(ascii_table(headers, rows), CodeWrap::Fence(None))
        }
        Block::Rule => RenderedBlock::prose("──────────".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::format::ast::parse;

    fn light(src: &str, links: LinkStyle) -> String {
        render(&parse(src), links)
            .iter()
            .map(|b| b.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    #[test]
    fn bold_single_asterisk() {
        assert_eq!(light("**hi**", LinkStyle::Raw), "*hi*");
    }

    #[test]
    fn italic_underscore() {
        assert_eq!(light("_hi_", LinkStyle::Raw), "_hi_");
    }

    #[test]
    fn heading_becomes_bold() {
        assert_eq!(light("## Title", LinkStyle::Raw), "*Title*");
    }

    #[test]
    fn link_raw_is_text_paren_url() {
        assert_eq!(
            light("[docs](https://x.io)", LinkStyle::Raw),
            "docs (https://x.io)"
        );
    }

    #[test]
    fn link_slack_is_angle_pipe() {
        assert_eq!(
            light("[docs](https://x.io)", LinkStyle::Slack),
            "<https://x.io|docs>"
        );
    }

    #[test]
    fn slack_escapes_angle_brackets() {
        assert!(light("a < b", LinkStyle::Slack).contains("&lt;"));
    }

    #[test]
    fn one_rendered_block_per_input_block() {
        let blocks = parse("# a\n\np\n\n- x");
        assert_eq!(render(&blocks, LinkStyle::Raw).len(), blocks.len());
    }

    #[test]
    fn code_in_a_list_item_keeps_its_fence() {
        // A fenced code block inside a list item should keep its fence when
        // materialized via join_all, not drop it to inline bare code.
        // This mirrors the plain.rs test "code_in_a_list_item_keeps_its_indent"
        // which ensures that nested code blocks maintain their wrapper.
        let out = light("1. Run:\n\n   ```\n   cmd\n   ```", LinkStyle::Raw);
        assert!(
            out.contains("```"),
            "code fence should be preserved in list item: {}",
            out
        );
    }

    #[test]
    fn nested_list_is_indented_not_flattened() {
        // A nested list should be indented relative to the parent item.
        // This mirrors the plain.rs test "nested_list_is_indented_not_flattened"
        // which ensures that when a list item contains another list, the child
        // is indented, not flattened onto the parent line.
        let out = light("- a\n  - b", LinkStyle::Raw);
        assert!(
            out.contains('\n'),
            "nested list should have newlines: {}",
            out
        );
        // Find a line that starts with spaces followed by a bullet (the indented child)
        let has_indented_child = out
            .lines()
            .any(|line| line.starts_with(' ') && line.contains('•'));
        assert!(
            has_indented_child,
            "nested list child should be indented: {}",
            out
        );
    }
}
