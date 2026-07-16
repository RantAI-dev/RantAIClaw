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
        // A fence is not an escaping exemption: Slack wants `&`/`<`/`>` escaped
        // everywhere in message text, code blocks included. The body stays RAW
        // (no fence) — `split.rs` adds that, and re-adds it per chunk — so what
        // is handed over must already be final.
        Block::CodeBlock { code, .. } => RenderedBlock::code(
            maybe_escape(code.trim_end_matches('\n'), links),
            CodeWrap::Fence(None),
        ),
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
        // Escaped for the same reason as `CodeBlock`: the ASCII grid goes out as
        // message text, and Slack reads `&`/`<`/`>` in it whether or not a fence
        // surrounds it. Escaping AFTER `ascii_table` is deliberate — the column
        // widths must be measured on what the user sees, not on `&lt;`.
        Block::Table { headers, rows, .. } => RenderedBlock::code(
            maybe_escape(&ascii_table(headers, rows), links),
            CodeWrap::Fence(None),
        ),
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

    /// The message as the platform actually receives it.
    ///
    /// Unlike `light`, this materializes each block through `join_all`, so a
    /// code block arrives fenced rather than as the bare body `.text` holds.
    fn light_full(src: &str, links: LinkStyle) -> String {
        crate::channels::format::render_to_string(
            src,
            &crate::channels::format::RenderTarget::LightMarkup { links },
        )
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

    // Every block kind, not just the three that happen to be one-liners — see
    // the same guard in `html.rs` for why the narrow input is not enough.
    #[test]
    fn one_rendered_block_per_input_block() {
        let blocks = parse("# a\n\np\n\n- x\n\n```\nc\n```\n\n> q\n\n| A |\n|---|\n| 1 |\n\n---");
        assert_eq!(blocks.len(), 7, "input lost a block: {blocks:?}");
        assert_eq!(render(&blocks, LinkStyle::Raw).len(), blocks.len());
        assert_eq!(render(&blocks, LinkStyle::Slack).len(), blocks.len());
    }

    #[test]
    fn rule_becomes_a_horizontal_line() {
        assert_eq!(light("---", LinkStyle::Raw), "──────────");
    }

    #[test]
    fn table_becomes_an_aligned_ascii_grid_in_a_fence() {
        let out = light_full("| A | Bee |\n|---|---|\n| 1 | 2 |", LinkStyle::Raw);
        // Exact: the fence must be there (the body is RAW until `join_all`), and
        // the grid must be padded to equal width. A `contains("A")` would pass on
        // a table that silently rendered as prose.
        assert_eq!(out, "```\nA | Bee\n--+----\n1 | 2  \n```");
    }

    // The Slack link branch escapes the URL with `escape_slack(url)`, which
    // nothing covered: every other Slack guard drives text, not a destination.
    // A query string is where this bites — `&` is the separator AND the thing
    // Slack wants escaped.
    #[test]
    fn slack_escapes_a_link_url() {
        assert_eq!(
            light("[docs](http://x.io?a=1&b=2)", LinkStyle::Slack),
            "<http://x.io?a=1&amp;b=2|docs>"
        );
    }

    // The counterweight, mirroring the code-block pair above: WhatsApp/Zulip
    // render entities literally, so the same URL must arrive untouched there.
    #[test]
    fn raw_link_style_leaves_a_link_url_alone() {
        assert_eq!(
            light("[docs](http://x.io?a=1&b=2)", LinkStyle::Raw),
            "docs (http://x.io?a=1&b=2)"
        );
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
    fn code_in_a_nested_list_item_keeps_its_fence() {
        // NOT a second, distinct `join_all`: this pins the SAME call site the
        // single-level test above does — the List arm's — just one level down.
        // The OUTER item's sub-blocks are `[Paragraph, List]`, which render to
        // `[Prose, Prose]`, and `materialize` for Prose is literally
        // `text.clone()`; at the outer level `join_all` and `.text` are
        // byte-identical and discriminate nothing. The fence survives only
        // because the List arm called `join_all` on the INNER item, where the
        // Code sub-block still exists as Code. (Swapping the arm to `.text`
        // fails this test AND the single-level one, for that one reason.)
        //
        // What it adds over the single-level test is the second pass: the fence
        // must survive being indented again as the outer item's continuation.
        let out = light("- a\n  - b\n\n    ```\n    cmd\n    ```", LinkStyle::Raw);
        assert!(
            out.contains("```"),
            "code fence should be preserved in a nested list item: {}",
            out
        );
    }

    #[test]
    fn blockquote_code_keeps_its_fence() {
        // Mirrors markdown.rs's `fenced_code_in_a_blockquote_keeps_its_fence`.
        // The BlockQuote handler also calls `join_all`, not `.text` — a fenced
        // code block quoted here must keep its fence, not get inlined bare.
        let out = light("> ```\n> cmd\n> ```", LinkStyle::Raw);
        assert!(
            out.contains("```"),
            "code fence should be preserved in a blockquote: {}",
            out
        );
    }

    // --- Slack escaping ------------------------------------------------------
    //
    // Slack requires `&`/`<`/`>` to be escaped everywhere in message text — a
    // code fence is not an exemption, it is still message text. Paragraphs,
    // inline code, blockquotes and list items already escaped; code blocks and
    // tables did not, so the SAME characters survived or not purely by which
    // block they landed in.

    const SCRIPT: &str = "```\n<script>a&b</script>\n```";

    #[test]
    fn slack_escapes_a_code_block_body() {
        let out = light_full(SCRIPT, LinkStyle::Slack);
        assert!(
            out.contains("&lt;script&gt;a&amp;b&lt;/script&gt;"),
            "code block left unescaped: {out}"
        );
        assert!(!out.contains("<script>"), "raw markup reached Slack: {out}");
    }

    #[test]
    fn slack_escapes_table_cells() {
        let out = light_full("| A | B |\n|---|---|\n| <b> | a&b |", LinkStyle::Slack);
        assert!(out.contains("&lt;b&gt;"), "table cell unescaped: {out}");
        assert!(out.contains("a&amp;b"), "table cell unescaped: {out}");
        assert!(!out.contains("<b>"), "raw markup reached Slack: {out}");
    }

    // The counterweight: `escape_slack` is Slack's dialect, not the renderer's.
    // WhatsApp and Zulip render HTML entities literally, so escaping for them
    // would show the reader `&lt;script&gt;` — the bug, inverted.
    #[test]
    fn raw_link_style_leaves_a_code_block_body_alone() {
        let out = light_full(SCRIPT, LinkStyle::Raw);
        assert!(out.contains("<script>a&b</script>"), "body altered: {out}");
        assert!(
            !out.contains("&lt;"),
            "entity leaked to WhatsApp/Zulip: {out}"
        );
        assert!(
            !out.contains("&amp;"),
            "entity leaked to WhatsApp/Zulip: {out}"
        );
    }

    // Escaping must happen AFTER `ascii_table` lays the grid out, not before.
    // The table is padded to the widest cell, and Slack renders `&lt;` back to
    // the single char `<` — so measuring the 9-char entity instead of the 3-char
    // `<b>` the reader sees pads every other column to a width that only exists
    // in the wire text, and the grid arrives visibly crooked.
    #[test]
    fn slack_table_escapes_after_layout_so_the_grid_stays_aligned() {
        let out = light_full("| A | B |\n|---|---|\n| <b> | x |", LinkStyle::Slack);
        // Undo the escaping to see the table as Slack renders it.
        let seen = out
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        let rows: Vec<&str> = seen.lines().filter(|l| !l.starts_with("```")).collect();
        let width = rows[0].chars().count();
        assert!(
            rows.iter().all(|r| r.chars().count() == width),
            "rendered grid is ragged, so layout measured the entities: {rows:?}"
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
