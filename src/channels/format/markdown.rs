//! StdMarkdown renderer: keep CommonMark-ish markup (`**bold**`, `#` headings,
//! `[t](u)`, fenced code). Tables stay native (`| a | b |`) for platforms that
//! render them (Mattermost) or become ASCII in a fenced block for those that do
//! not (Discord, DingTalk).

use super::ast::{Block, Inline, TableAlign};
use super::nest::{indent_continuation, prefix_lines};
use super::split::join_all;
use super::table::ascii_table;
use super::{CodeWrap, RenderedBlock};

fn inlines_md(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) => out.push_str(t),
            Inline::Code(t) => {
                out.push('`');
                out.push_str(t);
                out.push('`');
            }
            Inline::Strong(c) => {
                out.push_str("**");
                out.push_str(&inlines_md(c));
                out.push_str("**");
            }
            Inline::Emphasis(c) => {
                out.push('*');
                out.push_str(&inlines_md(c));
                out.push('*');
            }
            Inline::Strikethrough(c) => {
                out.push_str("~~");
                out.push_str(&inlines_md(c));
                out.push_str("~~");
            }
            Inline::Link { text, url } => {
                out.push('[');
                out.push_str(&inlines_md(text));
                out.push_str("](");
                out.push_str(url);
                out.push(')');
            }
            Inline::SoftBreak => out.push('\n'),
            Inline::HardBreak => out.push_str("  \n"),
        }
    }
    out
}

fn native_table(
    headers: &[Vec<Inline>],
    rows: &[Vec<Vec<Inline>>],
    align: &[TableAlign],
) -> String {
    let head = headers
        .iter()
        .map(|c| inlines_md(c))
        .collect::<Vec<_>>()
        .join(" | ");
    let sep = align
        .iter()
        .map(|a| match a {
            TableAlign::Left => ":---",
            TableAlign::Center => ":--:",
            TableAlign::Right => "---:",
            TableAlign::None => "---",
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let mut lines = vec![format!("| {head} |"), format!("| {sep} |")];
    for row in rows {
        let r = row
            .iter()
            .map(|c| inlines_md(c))
            .collect::<Vec<_>>()
            .join(" | ");
        lines.push(format!("| {r} |"));
    }
    lines.join("\n")
}

pub fn render(blocks: &[Block], tables_native: bool) -> Vec<RenderedBlock> {
    blocks
        .iter()
        .map(|b| render_block(b, tables_native))
        .collect()
}

fn render_block(block: &Block, tables_native: bool) -> RenderedBlock {
    match block {
        Block::Heading { level, inlines } => {
            let mut text = "#".repeat(*level as usize);
            text.push(' ');
            text.push_str(&inlines_md(inlines));
            RenderedBlock::prose(text)
        }
        Block::Paragraph(inlines) => RenderedBlock::prose(inlines_md(inlines)),
        Block::CodeBlock { lang, code } => {
            RenderedBlock::code(code.trim_end_matches('\n'), CodeWrap::Fence(lang.clone()))
        }
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
                    "- ".to_string()
                };
                // `join_all`, NOT `.text`: a `Code` sub-block holds the RAW body
                // with its fence deferred to `code_wrap`, so `.text` would drop
                // the fence and inline the snippet into the bullet.
                let body = join_all(&render(item, tables_native));
                let width = marker.chars().count();
                marker.push_str(&indent_continuation(body.trim_end(), width));
                parts.push(marker.trim_end().to_string());
            }
            RenderedBlock::prose(parts.join("\n"))
        }
        Block::BlockQuote(inner) => {
            let body = join_all(&render(inner, tables_native));
            RenderedBlock::prose(prefix_lines(&body, "> "))
        }
        Block::Table {
            headers,
            rows,
            align,
        } => {
            if tables_native {
                RenderedBlock::prose(native_table(headers, rows, align))
            } else {
                RenderedBlock::code(ascii_table(headers, rows), CodeWrap::Fence(None))
            }
        }
        Block::Rule => RenderedBlock::prose("---".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::format::ast::parse;

    fn md(src: &str, native: bool) -> String {
        render(&parse(src), native)
            .iter()
            .map(|b| b.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    #[test]
    fn keeps_bold_double_asterisk() {
        assert_eq!(md("**hi**", false), "**hi**");
    }

    #[test]
    fn keeps_heading_hashes() {
        assert_eq!(md("## Title", false), "## Title");
    }

    #[test]
    fn tight_list_renders_dashes() {
        assert_eq!(md("- a\n- b", false), "- a\n- b");
    }

    #[test]
    fn table_ascii_when_not_native() {
        let out = md("| A | B |\n|---|---|\n| 1 | 2 |", false);
        assert!(out.contains('A') && out.contains('-'));
        assert!(!out.contains("| A | B |"));
    }

    #[test]
    fn table_native_kept_as_pipes() {
        let out = md("| A | B |\n|---|---|\n| 1 | 2 |", true);
        assert!(out.contains("| A | B |"));
        assert!(out.contains("---"));
    }

    #[test]
    fn one_rendered_block_per_input_block() {
        let blocks = parse("# a\n\np\n\n- x");
        assert_eq!(render(&blocks, false).len(), blocks.len());
    }

    #[test]
    fn fenced_code_in_a_list_item_keeps_its_fence() {
        let out = md("1. Run:\n\n   ```bash\n   cmd\n   ```", false);
        assert!(out.contains("```bash"), "fence dropped: {out}");
    }

    #[test]
    fn nested_list_is_indented_not_flattened() {
        assert_eq!(md("- a\n  - b", false), "- a\n\n  - b");
    }

    #[test]
    fn fenced_code_in_a_blockquote_keeps_its_fence() {
        let out = md("> ```\n> cmd\n> ```", false);
        assert!(out.contains("```"), "fence dropped: {out}");
    }
}
