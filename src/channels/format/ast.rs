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

/// Max nesting depth of `Container` frames the builder will open.
///
/// This guards a **stack overflow, not a style preference**. `parse` itself is
/// safe at any depth (it is a `Vec` stack over a flat event stream), but every
/// renderer (`html`/`plain`/`markdown`/`light`) walks the resulting tree with
/// one Rust stack frame per `List`/`BlockQuote` level. Channels render from
/// async tasks on tokio workers, whose default stack is 2 MB — where a debug
/// build dies at ~1000 levels. A stack overflow is a `SIGABRT`, not a panic:
/// it is not catchable, so one deep reply would take the whole agent runtime
/// down (CLAUDE.md §7.3 forbids panics in the runtime path; an abort is worse).
///
/// 32 is ~4x the deepest nesting a real agent reply plausibly uses (a 3-4 level
/// list inside a quote) and ~30x below the measured ~1000-level failure point,
/// so it is invisible to real content while leaving a wide margin.
const MAX_CONTAINER_DEPTH: usize = 32;

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
    /// Count of container opens refused by [`MAX_CONTAINER_DEPTH`] that are
    /// still awaiting their matching `End`. See [`Builder::try_enter`].
    suppressed: usize,
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

    /// Whether a `List`/`Quote` may be opened at the current depth, recording
    /// the refusal when it may not. See [`MAX_CONTAINER_DEPTH`].
    ///
    /// The refusal is sticky: while `suppressed > 0` nothing is pushed and
    /// (because every container `End` is guarded) nothing is popped, so
    /// `block_stack.len()` cannot drop back under the cap mid-suppression.
    /// Refused opens are therefore always the innermost, contiguous region of
    /// the nesting, and their `End`s — which arrive innermost-first — are
    /// exactly the next `suppressed` container `End`s. That is what makes a
    /// plain counter, rather than a parallel stack, sound here.
    fn try_enter(&mut self) -> bool {
        if self.block_stack.len() >= MAX_CONTAINER_DEPTH {
            self.suppressed += 1;
            return false;
        }
        true
    }

    /// Whether an `Item` may be opened. It inherits its parent `List`'s
    /// decision instead of re-checking depth, which keeps the pair atomic: a
    /// pushed `List` always gets its `Item`s. Re-checking would let a `List`
    /// open at `MAX_CONTAINER_DEPTH - 1` while its `Item`s were refused, and
    /// `TagEnd::Item` files an item's children into the enclosing `List` — so
    /// that `List` would silently drop every item's content.
    fn try_enter_item(&mut self) -> bool {
        if self.suppressed > 0 {
            self.suppressed += 1;
            return false;
        }
        true
    }

    /// Symmetric to [`Builder::try_enter`]: `true` when this `End` closes a
    /// refused open and must therefore NOT pop a real frame.
    fn leave_suppressed(&mut self) -> bool {
        if self.suppressed > 0 {
            self.suppressed -= 1;
            return true;
        }
        false
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
            // `flush_implicit` runs whether or not the container is refused: it
            // closes any open tight-item text into a Paragraph in the enclosing
            // container FIRST, which is what keeps `- a\n  - b` as
            // `[Paragraph(a), List(b)]`, and on the refused path is what emits
            // over-deep text as siblings instead of merging it into one run.
            Tag::List(start) => {
                self.flush_implicit();
                if self.try_enter() {
                    self.block_stack.push(Container::List {
                        ordered: start.is_some(),
                        start: start.unwrap_or(1),
                        items: Vec::new(),
                    });
                }
            }
            Tag::Item => {
                if self.try_enter_item() {
                    self.block_stack.push(Container::Item(Vec::new()));
                }
            }
            Tag::BlockQuote(_) => {
                self.flush_implicit();
                if self.try_enter() {
                    self.block_stack.push(Container::Quote(Vec::new()));
                }
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
            // Each container `End` must mirror `start`'s refusal: popping a real
            // frame to close an open that was never pushed would desync the
            // stack, which is worse than the overflow this cap prevents.
            TagEnd::List(_) => {
                if self.leave_suppressed() {
                    return;
                }
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
                // Before the guard: a refused item's text still has to land in
                // the nearest surviving container, or over-deep content is lost.
                self.flush_implicit();
                if self.leave_suppressed() {
                    return;
                }
                if let Some(Container::Item(children)) = self.block_stack.pop() {
                    if let Some(Container::List { items, .. }) = self.block_stack.last_mut() {
                        items.push(children);
                    }
                }
            }
            TagEnd::BlockQuote(_) => {
                if self.leave_suppressed() {
                    return;
                }
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

    // --- MAX_CONTAINER_DEPTH -------------------------------------------------
    //
    // The cap exists to protect the RENDERERS: each recurses one Rust stack
    // frame per List/BlockQuote level, and channels render on 2 MB tokio worker
    // stacks where a debug build overflows at ~1000 levels. An overflow is a
    // SIGABRT, not a catchable panic, so it kills the whole runtime.
    //
    // Depth 200 is deliberate: it is ~6x the cap (so the cap must engage) yet
    // stays under the ~1000/~4000-level overflow thresholds, so if the cap ever
    // stopped binding these tests would FAIL an assertion rather than abort the
    // harness and take every other test's result with them.

    const OVER_CAP: usize = 200;

    /// Max nesting of renderer-recursive containers in a parsed tree. Recurses
    /// only over the already-capped AST, so it is itself depth-bounded.
    fn container_depth(blocks: &[Block]) -> usize {
        blocks
            .iter()
            .map(|b| match b {
                Block::BlockQuote(inner) => 1 + container_depth(inner),
                Block::List { items, .. } => {
                    1 + items.iter().map(|i| container_depth(i)).max().unwrap_or(0)
                }
                _ => 0,
            })
            .max()
            .unwrap_or(0)
    }

    fn nested_quotes(depth: usize) -> String {
        format!("{} deepest", ">".repeat(depth))
    }

    fn nested_lists(depth: usize) -> String {
        (0..depth)
            .map(|i| format!("{}- level{i}", " ".repeat(i * 2)))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
            + &" ".repeat(depth * 2)
            + "- deepest"
    }

    #[test]
    fn quotes_past_the_cap_stay_bounded_and_keep_their_text() {
        let blocks = parse(&nested_quotes(OVER_CAP));
        assert!(
            container_depth(&blocks) <= MAX_CONTAINER_DEPTH,
            "cap did not bind: depth {}",
            container_depth(&blocks)
        );
        let out = crate::channels::format::render_to_string(
            &nested_quotes(OVER_CAP),
            &crate::channels::format::RenderTarget::Plain,
        );
        assert!(out.contains("deepest"), "over-deep text was lost: {out:?}");
    }

    #[test]
    fn lists_past_the_cap_stay_bounded_and_keep_their_text() {
        let blocks = parse(&nested_lists(OVER_CAP));
        assert!(
            container_depth(&blocks) <= MAX_CONTAINER_DEPTH,
            "cap did not bind: depth {}",
            container_depth(&blocks)
        );
        let out = crate::channels::format::render_to_string(
            &nested_lists(OVER_CAP),
            &crate::channels::format::RenderTarget::Plain,
        );
        assert!(out.contains("deepest"), "over-deep text was lost: {out:?}");
        // Flattened, not dropped: every level's text still reaches the user.
        assert!(out.contains("level0") && out.contains("level199"));
    }

    // The normal path must be untouched: under the cap, shape is exact.
    #[test]
    fn nesting_just_under_the_cap_is_unaffected() {
        let blocks = parse(&nested_quotes(MAX_CONTAINER_DEPTH - 1));
        assert_eq!(container_depth(&blocks), MAX_CONTAINER_DEPTH - 1);

        // Peel every level: nothing refused, nothing flattened.
        let mut cur = &blocks;
        for _ in 0..MAX_CONTAINER_DEPTH - 1 {
            let [Block::BlockQuote(inner)] = cur.as_slice() else {
                panic!("expected a single blockquote, got {cur:?}")
            };
            cur = inner;
        }
        assert_eq!(
            cur.as_slice(),
            [Block::Paragraph(vec![Inline::Text("deepest".into())])]
        );
    }

    // Desync guard. Every refused open must have its `End` refused too. If the
    // counter under- or over-drained, the block_stack would not unwind back to
    // empty, and `push_block` would file this trailing paragraph into a
    // leftover container instead of at root — so `blocks` would not end with it
    // at top level. Covers both container kinds, since `List`/`Item` refuse via
    // different predicates (depth vs. inherited).
    #[test]
    fn container_stack_unwinds_after_refused_opens() {
        for deep in [nested_quotes(OVER_CAP), nested_lists(OVER_CAP)] {
            let blocks = parse(&format!("{deep}\n\nafter"));
            assert_eq!(
                blocks.last(),
                Some(&Block::Paragraph(vec![Inline::Text("after".into())])),
                "stack desynced: trailing block is {:?}",
                blocks.last()
            );
        }
    }

    /// Renders `blocks` to plain text so a test can assert on STRUCTURE, not
    /// just on the trailing block: `container_stack_unwinds_after_refused_opens`
    /// only inspects `blocks.last()`, and a trailing paragraph lands fine even
    /// while earlier content is being silently dropped. That blindness let a
    /// real content-deletion bug pass the whole suite.
    fn rendered(md: &str) -> String {
        crate::channels::format::render_to_string(md, &crate::channels::format::RenderTarget::Plain)
    }

    // A refused container's `End` must NOT pop a frame that was never pushed.
    // When `TagEnd::BlockQuote` skipped its `leave_suppressed()` guard, each
    // refused quote-End popped a real `Item`/`List` frame instead, which failed
    // the `Container::Quote` pattern and was dropped with all its children — and
    // `suppressed` never drained, poisoning every later list in the document.
    #[test]
    fn a_refused_quote_does_not_eat_its_enclosing_list_item() {
        // 31 is UNDER the cap on its own; the enclosing list item's frames are
        // what push it over, which is why this needs a quote inside a list.
        let quote = ">".repeat(31);
        let out = rendered(&format!("- BEFORE\n\n  {quote} dq\n\n- AFTER"));
        assert!(out.contains("BEFORE"), "list item deleted: {out:?}");
        assert!(out.contains("dq"), "quote text deleted: {out:?}");
        assert!(out.contains("AFTER"), "trailing item deleted: {out:?}");
    }

    #[test]
    fn a_refused_quote_does_not_poison_a_later_list() {
        let quote = ">".repeat(33);
        let out = rendered(&format!("{quote} q\n\n- a\n- b"));
        assert!(out.contains('q'), "quote text deleted: {out:?}");
        assert!(
            out.contains("• a") && out.contains("• b"),
            "later list flattened to bare paragraphs, so `suppressed` leaked: {out:?}"
        );
    }

    // The property that actually matters: the cap protects the renderers, so
    // every target must complete on over-deep input.
    #[test]
    fn all_targets_render_over_deep_input() {
        use crate::channels::format::{render_to_string, LinkStyle, RenderTarget};

        let targets = [
            RenderTarget::Plain,
            RenderTarget::TelegramHtml,
            RenderTarget::MatrixHtml,
            RenderTarget::StdMarkdown {
                tables_native: true,
            },
            RenderTarget::LightMarkup {
                links: LinkStyle::Slack,
            },
        ];
        for md in [nested_quotes(OVER_CAP), nested_lists(OVER_CAP)] {
            for target in &targets {
                let out = render_to_string(&md, target);
                assert!(
                    out.contains("deepest"),
                    "{target:?} lost the over-deep text: {out:?}"
                );
            }
        }
    }

    /// The original defect, reproduced at the size that killed the runtime, on
    /// a tokio-worker-sized stack.
    ///
    /// `#[ignore]`d on purpose: it asserts the ABSENCE of a process abort. If
    /// the cap ever regresses this does not fail — it SIGABRTs the whole test
    /// harness and destroys every other test's result. Opt-in:
    /// `cargo test --lib channels::format -- --ignored`.
    #[test]
    #[ignore = "asserts the absence of a SIGABRT: a regression aborts the harness \
                instead of failing, destroying every other test's result"]
    fn deep_quote_renders_on_worker_sized_stack() {
        let md = nested_quotes(2000);
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024) // tokio worker default
            .spawn(move || {
                let out = crate::channels::format::render_to_string(
                    &md,
                    &crate::channels::format::RenderTarget::Plain,
                );
                assert!(out.contains("deepest"));
            })
            .unwrap()
            .join()
            .expect("renderer overflowed a 2 MB worker stack");
    }
}
