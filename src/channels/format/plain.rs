//! Plain-text renderer: strip all markup to readable text. Headings become an
//! UPPERCASED line; tables become aligned ASCII; links become `text (url)`;
//! code keeps its text (indented, for fence-averse platforms).

use super::ast::{Block, Inline};
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
            let mut text = String::new();
            for (i, item) in items.iter().enumerate() {
                if *ordered {
                    let n = usize::try_from(*start).unwrap_or(1) + i;
                    text.push_str(&n.to_string());
                    text.push_str(". ");
                } else {
                    text.push_str("• ");
                }
                let body = render(item)
                    .iter()
                    .map(|b| b.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n");
                text.push_str(body.trim());
                text.push('\n');
            }
            RenderedBlock::prose(text.trim_end().to_string())
        }
        Block::BlockQuote(inner) => {
            let quoted = render(inner)
                .iter()
                .map(|b| {
                    b.text
                        .lines()
                        .map(|l| {
                            let mut s = String::from("> ");
                            s.push_str(l);
                            s
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect::<Vec<_>>()
                .join("\n");
            RenderedBlock::prose(quoted)
        }
        Block::Table { headers, rows, .. } => {
            RenderedBlock::code(ascii_table(headers, rows), CodeWrap::Indent)
        }
        Block::Rule => RenderedBlock::prose("—".repeat(10)),
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
}
