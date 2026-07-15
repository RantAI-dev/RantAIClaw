//! GFM → a small block AST that every renderer walks.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TableAlign {
    None,
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Code(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Link { text: Vec<Inline>, url: String },
    SoftBreak,
    HardBreak,
}

// `CodeBlock`/`BlockQuote` are the CommonMark domain names; renaming them to
// satisfy `enum_variant_names` would make the AST harder to map back to the spec.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    CodeBlock {
        lang: Option<String>,
        code: String,
    },
    List {
        ordered: bool,
        start: u64,
        items: Vec<Vec<Block>>,
    },
    BlockQuote(Vec<Block>),
    Table {
        align: Vec<TableAlign>,
        headers: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    Rule,
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn align_of(a: Alignment) -> TableAlign {
    match a {
        Alignment::None => TableAlign::None,
        Alignment::Left => TableAlign::Left,
        Alignment::Center => TableAlign::Center,
        Alignment::Right => TableAlign::Right,
    }
}

/// Parse `md` as GFM (tables + strikethrough enabled) into blocks.
pub fn parse(md: &str) -> Vec<Block> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    Builder::default().run(Parser::new_ext(md, opts))
}

enum Container {
    List {
        ordered: bool,
        start: u64,
        items: Vec<Vec<Block>>,
    },
    Item(Vec<Block>),
    Quote(Vec<Block>),
}

struct TableBuild {
    align: Vec<TableAlign>,
    headers: Vec<Vec<Inline>>,
    rows: Vec<Vec<Vec<Inline>>>,
    in_head: bool,
    current_row: Vec<Vec<Inline>>,
}

/// Stack-based tree builder over the flat pulldown-cmark event stream.
#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    /// Inline accumulation stack; the top frame collects the current run.
    inline_stack: Vec<Vec<Inline>>,
    block_stack: Vec<Container>,
    code: Option<(Option<String>, String)>,
    table: Option<TableBuild>,
    link_urls: Vec<String>,
}

impl Builder {
    fn run(mut self, parser: Parser) -> Vec<Block> {
        for event in parser {
            self.event(event);
        }
        self.blocks
    }

    fn push_block(&mut self, block: Block) {
        match self.block_stack.last_mut() {
            Some(Container::Item(children) | Container::Quote(children)) => children.push(block),
            _ => self.blocks.push(block),
        }
    }

    /// Auto-open a frame: tight list items and HTML blocks deliver text with no
    /// enclosing Paragraph tag, and that text must not be dropped.
    fn push_inline(&mut self, inline: Inline) {
        if self.inline_stack.is_empty() {
            self.inline_stack.push(Vec::new());
        }
        if let Some(top) = self.inline_stack.last_mut() {
            top.push(inline);
        }
    }

    /// Close an implicitly-opened run into a Paragraph.
    fn flush_implicit(&mut self) {
        if let Some(inlines) = self.inline_stack.pop() {
            if !inlines.is_empty() {
                self.push_block(Block::Paragraph(inlines));
            }
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some((_, code)) = self.code.as_mut() {
                    code.push_str(&t);
                } else {
                    self.push_inline(Inline::Text(t.to_string()));
                }
            }
            Event::Code(t) => self.push_inline(Inline::Code(t.to_string())),
            Event::SoftBreak => self.push_inline(Inline::SoftBreak),
            Event::HardBreak => self.push_inline(Inline::HardBreak),
            Event::Rule => self.push_block(Block::Rule),
            // Keep the literal text of HTML/inline-HTML; drop the markup itself.
            Event::Html(t) | Event::InlineHtml(t) => self.push_inline(Inline::Text(t.to_string())),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            // One arm: `match_same_arms` (pedantic) rejects splitting these into
            // two arms with identical bodies.
            Tag::Paragraph
            | Tag::Heading { .. }
            | Tag::HtmlBlock
            | Tag::TableCell
            | Tag::Emphasis
            | Tag::Strong
            | Tag::Strikethrough => self.inline_stack.push(Vec::new()),
            Tag::CodeBlock(kind) => {
                // A tight list item's text may still be open — flush it first.
                self.flush_implicit();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let l = info.split_whitespace().next().unwrap_or("");
                        (!l.is_empty()).then(|| l.to_string())
                    }
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush_implicit();
                self.block_stack.push(Container::List {
                    ordered: start.is_some(),
                    start: start.unwrap_or(1),
                    items: Vec::new(),
                });
            }
            Tag::Item => self.block_stack.push(Container::Item(Vec::new())),
            Tag::BlockQuote(_) => {
                self.flush_implicit();
                self.block_stack.push(Container::Quote(Vec::new()));
            }
            Tag::Table(aligns) => {
                self.table = Some(TableBuild {
                    align: aligns.into_iter().map(align_of).collect(),
                    headers: Vec::new(),
                    rows: Vec::new(),
                    in_head: false,
                    current_row: Vec::new(),
                });
            }
            Tag::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.in_head = true;
                }
            }
            Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.current_row = Vec::new();
                }
            }
            Tag::Link { dest_url, .. } => {
                self.inline_stack.push(Vec::new());
                self.link_urls.push(dest_url.to_string());
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                let inlines = self.inline_stack.pop().unwrap_or_default();
                self.push_block(Block::Paragraph(inlines));
            }
            TagEnd::Heading(level) => {
                let inlines = self.inline_stack.pop().unwrap_or_default();
                self.push_block(Block::Heading {
                    level: heading_level(level),
                    inlines,
                });
            }
            // Tight list items and HTML blocks deliver text without Paragraph tags.
            TagEnd::HtmlBlock => self.flush_implicit(),
            TagEnd::CodeBlock => {
                if let Some((lang, code)) = self.code.take() {
                    self.push_block(Block::CodeBlock { lang, code });
                }
            }
            TagEnd::List(_) => {
                if let Some(Container::List {
                    ordered,
                    start,
                    items,
                }) = self.block_stack.pop()
                {
                    self.push_block(Block::List {
                        ordered,
                        start,
                        items,
                    });
                }
            }
            TagEnd::Item => {
                self.flush_implicit();
                if let Some(Container::Item(children)) = self.block_stack.pop() {
                    if let Some(Container::List { items, .. }) = self.block_stack.last_mut() {
                        items.push(children);
                    }
                }
            }
            TagEnd::BlockQuote(_) => {
                if let Some(Container::Quote(children)) = self.block_stack.pop() {
                    self.push_block(Block::BlockQuote(children));
                }
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.push_block(Block::Table {
                        align: t.align,
                        headers: t.headers,
                        rows: t.rows,
                    });
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.in_head = false;
                    t.headers = std::mem::take(&mut t.current_row);
                }
            }
            TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    if !t.in_head {
                        let row = std::mem::take(&mut t.current_row);
                        t.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                let cell = self.inline_stack.pop().unwrap_or_default();
                if let Some(t) = self.table.as_mut() {
                    t.current_row.push(cell);
                }
            }
            TagEnd::Emphasis => self.wrap_inline(Inline::Emphasis),
            TagEnd::Strong => self.wrap_inline(Inline::Strong),
            TagEnd::Strikethrough => self.wrap_inline(Inline::Strikethrough),
            TagEnd::Link => {
                let text = self.inline_stack.pop().unwrap_or_default();
                let url = self.link_urls.pop().unwrap_or_default();
                self.push_inline(Inline::Link { text, url });
            }
            _ => {}
        }
    }

    fn wrap_inline(&mut self, f: fn(Vec<Inline>) -> Inline) {
        let children = self.inline_stack.pop().unwrap_or_default();
        self.push_inline(f(children));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_heading_and_paragraph() {
        let blocks = parse("## Title\n\nHello **world**");
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], Block::Heading { level: 2, .. }));
        assert!(matches!(&blocks[1], Block::Paragraph(_)));
    }

    #[test]
    fn parses_fenced_code_with_lang() {
        let blocks = parse("```python\nprint(1)\n```");
        match &blocks[0] {
            Block::CodeBlock { lang, code } => {
                assert_eq!(lang.as_deref(), Some("python"));
                assert_eq!(code, "print(1)\n");
            }
            other => panic!("expected code block, got {other:?}"),
        }
    }

    #[test]
    fn parses_table_headers_and_rows() {
        let blocks = parse("| A | B |\n|---|---|\n| 1 | 2 |");
        match &blocks[0] {
            Block::Table { headers, rows, .. } => {
                assert_eq!(headers.len(), 2);
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].len(), 2);
            }
            other => panic!("expected table, got {other:?}"),
        }
    }

    #[test]
    fn strong_emphasis_strike_inline() {
        let blocks = parse("**b** _i_ ~~s~~");
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!("expected paragraph")
        };
        assert!(inlines.iter().any(|i| matches!(i, Inline::Strong(_))));
        assert!(inlines.iter().any(|i| matches!(i, Inline::Emphasis(_))));
        assert!(inlines
            .iter()
            .any(|i| matches!(i, Inline::Strikethrough(_))));
    }

    // Regression: tight list items carry no Paragraph tags — their text must not
    // be dropped.
    #[test]
    fn tight_list_items_keep_their_text() {
        let blocks = parse("- a\n- b");
        let Block::List { items, .. } = &blocks[0] else {
            panic!("expected list")
        };
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0],
            vec![Block::Paragraph(vec![Inline::Text("a".into())])]
        );
        assert_eq!(
            items[1],
            vec![Block::Paragraph(vec![Inline::Text("b".into())])]
        );
    }

    #[test]
    fn nested_list_keeps_outer_text_and_inner_list() {
        let blocks = parse("- a\n  - b");
        let Block::List { items, .. } = &blocks[0] else {
            panic!("expected list")
        };
        assert_eq!(items[0].len(), 2);
        assert_eq!(
            items[0][0],
            Block::Paragraph(vec![Inline::Text("a".into())])
        );
        assert!(matches!(items[0][1], Block::List { .. }));
    }

    // Regression: block-level HTML must not be dropped.
    #[test]
    fn html_block_text_is_kept() {
        let blocks = parse("<script>x</script>");
        assert_eq!(blocks.len(), 1);
        let Block::Paragraph(inlines) = &blocks[0] else {
            panic!("expected paragraph")
        };
        assert!(!inlines.is_empty());
    }

    #[test]
    fn ordered_list_start_is_kept() {
        let blocks = parse("3. a\n4. b");
        let Block::List {
            ordered,
            start,
            items,
        } = &blocks[0]
        else {
            panic!("expected list")
        };
        assert!(ordered);
        assert_eq!(*start, 3);
        assert_eq!(items.len(), 2);
    }
}
