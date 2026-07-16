//! Plain-text renderer: strip all markup to readable text. Headings become an
//! UPPERCASED line; tables become aligned ASCII; links become `text (url)`;
//! code keeps its text (indented, for fence-averse platforms).

use super::ast::{Block, Inline};
use super::nest::{indent_continuation, prefix_lines};
use super::split::join_all;
use super::table::ascii_table;
use super::{CodeWrap, RenderedBlock};

/// Like `table::inline_plain`, but keeps link URLs as `text (url)`.
fn inline_text(inlines: &[Inline]) -> String {
    let mut out = String::new();
    for inline in inlines {
        match inline {
            Inline::Text(t) | Inline::Code(t) => out.push_str(t),
            Inline::Strong(c) | Inline::Emphasis(c) | Inline::Strikethrough(c) => {
                out.push_str(&inline_text(c));
            }
            Inline::Link { text, url } => {
                out.push_str(&inline_text(text));
                out.push_str(" (");
                out.push_str(url);
                out.push(')');
            }
            Inline::SoftBreak | Inline::HardBreak => out.push(' '),
        }
    }
    out
}

pub fn render(blocks: &[Block]) -> Vec<RenderedBlock> {
    blocks.iter().map(render_block).collect()
}

fn render_block(block: &Block) -> RenderedBlock {
    match block {
        Block::Heading { inlines, .. } => RenderedBlock::prose(inline_text(inlines).to_uppercase()),
        Block::Paragraph(inlines) => RenderedBlock::prose(inline_text(inlines)),
        Block::CodeBlock { code, .. } => {
            RenderedBlock::code(code.trim_end_matches('\n'), CodeWrap::Indent)
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
                    "• ".to_string()
                };
                // `join_all`, NOT `.text`: a `Code` sub-block holds the RAW body
                // with its fence deferred to `code_wrap`, so `.text` would drop
                // the fence and inline the snippet into the bullet.
                let body = join_all(&render(item));
                let width = marker.chars().count();
                marker.push_str(&indent_continuation(body.trim_end(), width));
                parts.push(marker.trim_end().to_string());
            }
            RenderedBlock::prose(parts.join("\n"))
        }
        Block::BlockQuote(inner) => {
            let body = join_all(&render(inner));
            RenderedBlock::prose(prefix_lines(&body, "> "))
        }
        Block::Table { headers, rows, .. } => {
            RenderedBlock::code(ascii_table(headers, rows), CodeWrap::Indent)
        }
        // The same box-drawing glyph Telegram and LightMarkup use. This arm
        // diverged with an em dash (`—`), which is neither more portable —
        // Plain already emits `•` above, so it was never ASCII-only — nor a
        // better rule: repeated em dashes leave visible gaps in most fonts
        // where `─` joins into a continuous line. `tests.rs` pins the agreement.
        Block::Rule => RenderedBlock::prose("──────────".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::format::ast::parse;
    use crate::channels::format::BlockKind;

    fn joined(md: &str) -> String {
        render(&parse(md))
            .iter()
            .map(|b| b.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    #[test]
    fn heading_becomes_upper_line() {
        assert_eq!(joined("## Penjelasan Detail"), "PENJELASAN DETAIL");
    }

    #[test]
    fn bold_and_italic_stripped() {
        assert_eq!(joined("**bold** _it_"), "bold it");
    }

    #[test]
    fn link_becomes_text_paren_url() {
        assert_eq!(joined("[docs](https://x.io)"), "docs (https://x.io)");
    }

    #[test]
    fn nested_link_keeps_url() {
        assert_eq!(joined("**[docs](https://x.io)**"), "docs (https://x.io)");
    }

    #[test]
    fn tight_list_renders_bullets() {
        assert_eq!(joined("- a\n- b"), "• a\n• b");
    }

    #[test]
    fn code_block_kept_as_code_kind() {
        let blocks = render(&parse("```python\nprint(1)\n```"));
        assert_eq!(blocks[0].kind, BlockKind::Code);
    }

    #[test]
    fn table_flattened_to_ascii() {
        let out = joined("| A | B |\n|---|---|\n| 1 | 2 |");
        assert!(out.contains('A') && out.contains('-'));
    }

    #[test]
    fn one_rendered_block_per_input_block() {
        let blocks = parse("# a\n\np\n\n- x\n\n```\nc\n```");
        assert_eq!(render(&blocks).len(), blocks.len());
    }

    #[test]
    fn nested_list_is_indented_not_flattened() {
        assert_eq!(joined("- a\n  - b"), "• a\n\n  • b");
    }

    // `contains("    cmd")` pins neither the column nor the width: it matches on
    // ANY indent of four or more, so it passes on the marker's 3-space
    // continuation alone and cannot tell the wrapper's 4 from the 7 the two
    // compose to. The exact string pins both.
    #[test]
    fn code_in_a_list_item_keeps_its_indent() {
        let out = joined("1. Run:\n\n   ```\n   cmd\n   ```");
        assert_eq!(out, "1. Run:\n\n       cmd");
    }
}
