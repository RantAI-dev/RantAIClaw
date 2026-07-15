//! Pack rendered blocks into platform-sized chunks without cutting a code fence
//! or an HTML tag. Prose is packed greedily and hard-split at word boundaries
//! only when a single block exceeds the limit; HTML prose is split only at tag
//! depth 0 so tags stay balanced by construction; code is re-wrapped per chunk
//! so a fence is never orphaned.

use super::{BlockKind, CodeWrap, RenderedBlock};

const SEP: &str = "\n\n";

fn wrap_code(body: &str, wrap: &CodeWrap) -> String {
    match wrap {
        CodeWrap::Fence(lang) => {
            let mut out = String::from("```");
            if let Some(l) = lang {
                out.push_str(l);
            }
            out.push('\n');
            out.push_str(body);
            out.push_str("\n```");
            out
        }
        CodeWrap::HtmlPre => {
            let mut out = String::from("<pre>");
            out.push_str(body);
            out.push_str("</pre>");
            out
        }
        CodeWrap::Indent => body
            .lines()
            .map(|l| {
                let mut s = String::from("    ");
                s.push_str(l);
                s
            })
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn wrap_overhead(wrap: &CodeWrap) -> usize {
    match wrap {
        CodeWrap::Fence(lang) => 8 + lang.as_deref().unwrap_or("").chars().count(),
        CodeWrap::HtmlPre => 11,
        CodeWrap::Indent => 4,
    }
}

fn materialize(block: &RenderedBlock) -> String {
    match block.kind {
        BlockKind::Prose | BlockKind::ProseHtml => block.text.clone(),
        BlockKind::Code => {
            let wrap = block.code_wrap.clone().unwrap_or(CodeWrap::Fence(None));
            wrap_code(&block.text, &wrap)
        }
    }
}

pub fn join_all(blocks: &[RenderedBlock]) -> String {
    blocks.iter().map(materialize).collect::<Vec<_>>().join(SEP)
}

/// Split one oversized block into limit-respecting pieces.
fn split_oversized(block: &RenderedBlock, limit: usize) -> Vec<String> {
    match block.kind {
        BlockKind::Code => {
            let wrap = block.code_wrap.clone().unwrap_or(CodeWrap::Fence(None));
            let budget = limit.saturating_sub(wrap_overhead(&wrap)).max(1);
            pack_lines(&block.text, budget)
                .iter()
                .map(|piece| wrap_code(piece, &wrap))
                .collect()
        }
        BlockKind::ProseHtml => hard_split_html(&block.text, limit),
        BlockKind::Prose => hard_split(&block.text, limit),
    }
}

pub fn split(blocks: &[RenderedBlock], limit: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();

    for block in blocks {
        let rendered = materialize(block);
        let addition = rendered.chars().count()
            + if current.is_empty() {
                0
            } else {
                SEP.chars().count()
            };

        if !current.is_empty() && current.chars().count() + addition > limit {
            chunks.push(std::mem::take(&mut current));
        }

        if rendered.chars().count() <= limit {
            if !current.is_empty() {
                current.push_str(SEP);
            }
            current.push_str(&rendered);
            continue;
        }

        if !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        chunks.extend(split_oversized(block, limit));
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(String::new());
    }
    chunks
}

/// Split `primary` into chunks, carrying the matching `fallback` rendering of the
/// SAME blocks alongside each chunk.
///
/// Zipping two independently-split renderings by index is unsound: they have
/// different lengths and therefore different chunk boundaries and counts. Instead
/// every packing decision is made on `primary`'s lengths and the fallback text of
/// the same block range travels with it, so both members of a pair always cover
/// identical source blocks.
///
/// `primary` and `fallback` MUST be 1:1 (the renderer invariant). Every renderer
/// is `blocks.iter().map(…).collect()`, so this holds structurally for anything
/// built by `render_pair`. A mismatch is a bug: `debug_assert_eq!` panics in
/// debug; in release, a longer `primary` yields empty fallbacks (safe — the
/// caller bails) while a longer `fallback` silently drops its surplus.
///
/// A pair's fallback is empty ONLY when no sound twin exists — see the oversized
/// branch below. Callers MUST treat an empty fallback as "no fallback available"
/// and never send the primary unrendered.
pub fn split_paired(
    primary: &[RenderedBlock],
    fallback: &[RenderedBlock],
    limit: usize,
) -> Vec<(String, String)> {
    debug_assert_eq!(
        primary.len(),
        fallback.len(),
        "renderers must emit one block per input block"
    );

    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut cur_p = String::new();
    let mut cur_f = String::new();

    for (i, block) in primary.iter().enumerate() {
        let rendered = materialize(block);
        let fb = fallback.get(i).map(materialize).unwrap_or_default();

        let sep = SEP.chars().count();
        let p_add = rendered.chars().count() + if cur_p.is_empty() { 0 } else { sep };
        let f_add = fb.chars().count() + if cur_f.is_empty() { 0 } else { sep };

        // Measure BOTH members. "HTML is always >= Plain" does not hold: a Plain
        // table is CodeWrap::Indent (+4 chars per line) against HtmlPre (+11
        // total), and Plain headings uppercase (`ß` -> `SS`). An unmeasured twin
        // can overflow the limit and be rejected just like the primary.
        let p_over = !cur_p.is_empty() && cur_p.chars().count() + p_add > limit;
        let f_over = !cur_f.is_empty() && cur_f.chars().count() + f_add > limit;
        if p_over || f_over {
            pairs.push((std::mem::take(&mut cur_p), std::mem::take(&mut cur_f)));
        }

        if rendered.chars().count() <= limit && fb.chars().count() <= limit {
            if !cur_p.is_empty() {
                cur_p.push_str(SEP);
            }
            // Guard the fallback separator on ITS own emptiness: an empty twin
            // would otherwise leave cur_f leading with a stray "\n\n".
            if !cur_f.is_empty() {
                cur_f.push_str(SEP);
            }
            cur_p.push_str(&rendered);
            cur_f.push_str(&fb);
            continue;
        }

        if !cur_p.is_empty() || !cur_f.is_empty() {
            pairs.push((std::mem::take(&mut cur_p), std::mem::take(&mut cur_f)));
        }

        // Oversized block: emit the primary's pieces with NO twin.
        //
        // Splitting `fallback[i]` independently and re-zipping by index is the
        // same unsound zip this function exists to remove, merely scoped to one
        // block: the two split at different boundaries into different counts, so
        // piece `n` of one does not cover piece `n` of the other. That produces
        // duplicated content (a twin covering source its primary does not) and
        // silent truncation (surplus fallback pieces dropped). An empty twin
        // makes the caller bail loudly instead — the honest outcome, and only
        // reachable when a single block exceeds the limit AND the platform
        // rejects the primary.
        for piece in split_oversized(block, limit) {
            pairs.push((piece, String::new()));
        }
    }

    if !cur_p.is_empty() || !cur_f.is_empty() {
        pairs.push((cur_p, cur_f));
    }
    if pairs.is_empty() {
        pairs.push((String::new(), String::new()));
    }
    pairs
}

/// Greedily pack whole lines under `budget` chars.
fn pack_lines(text: &str, budget: usize) -> Vec<String> {
    let budget = budget.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        let add = line.chars().count() + usize::from(!cur.is_empty());
        if !cur.is_empty() && cur.chars().count() + add > budget {
            out.push(std::mem::take(&mut cur));
        }
        if line.chars().count() > budget {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            out.extend(hard_split(line, budget));
        } else {
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(line);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Hard-split at word boundaries, then char boundaries, so no piece exceeds
/// `limit`. `limit` is clamped to >= 1: a 0 limit would loop forever.
fn hard_split(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        let add = word.chars().count() + usize::from(!cur.is_empty());
        if !cur.is_empty() && cur.chars().count() + add > limit {
            out.push(std::mem::take(&mut cur));
        }
        if word.chars().count() > limit {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            let mut chars = word.chars().peekable();
            while chars.peek().is_some() {
                out.push(chars.by_ref().take(limit).collect::<String>());
            }
        } else {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Split HTML prose only between top-level elements, so `<b>…</b>` pairs are
/// never separated.
///
/// Depth counts *elements*, not `<`/`>` characters: text nodes are already
/// escaped (`escape_html` turns a literal `<` into `&lt;`), so every `<` here
/// opens a real tag — but a naive `+1` on `<` and `-1` on `>` returns to 0
/// *inside* `<b>…</b>`, making its spaces look like safe break points and
/// yielding unbalanced output. Each tag is therefore consumed whole and
/// classified: closing tags decrement, void tags (`<br>`, `<hr>`, `… />`) do
/// nothing, everything else increments.
///
/// A single element run longer than `limit` is atomic — it cannot be split
/// without breaking its tags — so it is emitted oversized. See
/// `html_prose_oversized_run_stays_balanced`.
fn hard_split_html(text: &str, limit: usize) -> Vec<String> {
    let limit = limit.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut pending = String::new();
    let mut depth: i32 = 0;
    let mut chars = text.chars().peekable();

    let flush = |pending: &mut String, cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() && cur.chars().count() + pending.chars().count() > limit {
            out.push(std::mem::take(cur));
        }
        if cur.is_empty() && pending.chars().count() > limit {
            if pending.contains('<') {
                // One atomic element run longer than the limit — splitting it
                // would unbalance its tags, so it goes out oversized.
                out.push(std::mem::take(pending));
            } else {
                // Plain text between elements: safe to break at words.
                out.extend(hard_split(pending, limit));
                pending.clear();
            }
            return;
        }
        cur.push_str(pending);
        pending.clear();
    };

    while let Some(ch) = chars.next() {
        if ch == '<' {
            let closing = chars.peek() == Some(&'/');
            let mut tag = String::from('<');
            for c in chars.by_ref() {
                tag.push(c);
                if c == '>' {
                    break;
                }
            }
            let void = tag.ends_with("/>") || tag.starts_with("<br") || tag.starts_with("<hr");
            if closing {
                depth = (depth - 1).max(0);
            } else if !void {
                depth += 1;
            }
            pending.push_str(&tag);
            continue;
        }
        pending.push(ch);
        if (ch == ' ' || ch == '\n') && depth == 0 {
            flush(&mut pending, &mut cur, &mut out);
        }
    }
    if !pending.is_empty() {
        flush(&mut pending, &mut cur, &mut out);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::format::{CodeWrap, RenderedBlock};

    #[test]
    fn packs_small_blocks_into_one_chunk() {
        let blocks = vec![RenderedBlock::prose("a"), RenderedBlock::prose("b")];
        assert_eq!(split(&blocks, 100), vec!["a\n\nb".to_string()]);
    }

    #[test]
    fn code_block_gets_fenced() {
        let blocks = vec![RenderedBlock::code(
            "print(1)",
            CodeWrap::Fence(Some("python".into())),
        )];
        assert_eq!(split(&blocks, 100)[0], "```python\nprint(1)\n```");
    }

    #[test]
    fn oversized_code_refences_each_chunk() {
        let code = (0..50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let blocks = vec![RenderedBlock::code(code, CodeWrap::Fence(None))];
        let chunks = split(&blocks, 40);
        assert!(chunks.len() > 1);
        assert!(chunks
            .iter()
            .all(|c| c.starts_with("```") && c.trim_end().ends_with("```")));
        assert!(chunks.iter().all(|c| c.chars().count() <= 40));
    }

    #[test]
    fn every_chunk_within_limit() {
        let blocks = vec![RenderedBlock::prose("x ".repeat(100))];
        assert!(split(&blocks, 50).iter().all(|c| c.chars().count() <= 50));
    }

    #[test]
    fn empty_input_yields_one_empty_chunk() {
        // Matches the pre-existing telegram contract (telegram_split_empty_message).
        assert_eq!(split(&[], 100), vec![String::new()]);
    }

    #[test]
    fn html_prose_splits_between_elements_not_inside_them() {
        // Spaces INSIDE <b>…</b> are not break points — a naive `<`/`>` depth
        // counter returns to 0 inside the pair and splits here.
        let blocks = vec![RenderedBlock::prose_html(
            "<b>aaa bbb</b> <i>ccc ddd</i> tail",
        )];
        for chunk in split(&blocks, 12) {
            assert_eq!(chunk.matches("<b>").count(), chunk.matches("</b>").count());
            assert_eq!(chunk.matches("<i>").count(), chunk.matches("</i>").count());
        }
    }

    #[test]
    fn html_prose_void_tags_do_not_break_depth() {
        let blocks = vec![RenderedBlock::prose_html("<br>a b<hr>c d")];
        let chunks = split(&blocks, 6);
        // Void tags open nothing, so the spaces around them stay breakable.
        assert!(chunks.len() > 1);
    }

    #[test]
    fn html_prose_oversized_run_stays_balanced() {
        // A single element longer than the limit is atomic: it is emitted
        // oversized rather than split into unbalanced markup.
        let text = format!("<b>{}</b> tail", "y".repeat(80));
        let chunks = split(&[RenderedBlock::prose_html(text)], 50);
        for chunk in &chunks {
            assert_eq!(chunk.matches("<b>").count(), chunk.matches("</b>").count());
        }
        assert!(chunks.iter().any(|c| c.chars().count() > 50));
    }

    #[test]
    fn html_prose_breakable_text_respects_limit() {
        let blocks = vec![RenderedBlock::prose_html(
            "<b>a</b> <b>b</b> <b>c</b> <b>d</b> <b>e</b>",
        )];
        assert!(split(&blocks, 20).iter().all(|c| c.chars().count() <= 20));
    }

    #[test]
    fn hard_split_with_zero_limit_terminates() {
        // Guard against an infinite loop on a degenerate limit.
        let blocks = vec![RenderedBlock::prose("abc")];
        assert!(!split(&blocks, 0).is_empty());
    }

    #[test]
    fn paired_split_keeps_chunk_counts_equal() {
        let primary = vec![
            RenderedBlock::prose_html("<b>aaaa</b>"),
            RenderedBlock::prose_html("<b>bbbb</b>"),
        ];
        let fallback = vec![RenderedBlock::prose("aaaa"), RenderedBlock::prose("bbbb")];
        let pairs = split_paired(&primary, &fallback, 12);
        assert!(pairs.len() >= 2);
        for (html, plain) in &pairs {
            assert!(!html.is_empty());
            assert!(!plain.is_empty());
        }
    }

    #[test]
    fn paired_split_pairs_cover_same_blocks() {
        let primary = vec![
            RenderedBlock::prose_html("<b>a</b>"),
            RenderedBlock::prose_html("<b>b</b>"),
        ];
        let fallback = vec![RenderedBlock::prose("a"), RenderedBlock::prose("b")];
        let pairs = split_paired(&primary, &fallback, 100);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "<b>a</b>\n\n<b>b</b>");
        assert_eq!(pairs[0].1, "a\n\nb");
    }

    // An oversized block has no sound twin: splitting the fallback independently
    // and re-zipping by index duplicates and truncates content. The twin must be
    // empty so the caller bails instead of sending the wrong text.
    #[test]
    fn paired_split_oversized_block_has_no_twin() {
        let primary = vec![RenderedBlock::prose_html(
            "<i>aaaa</i> <i>bbbb</i> <i>cccc</i>",
        )];
        let fallback = vec![RenderedBlock::prose("aaaa bbbb cccc")];
        let pairs = split_paired(&primary, &fallback, 20);
        assert!(pairs.len() > 1);
        assert!(pairs.iter().all(|(_, f)| f.is_empty()));
    }

    #[test]
    fn paired_split_measures_the_fallback_too() {
        // Twin longer than the primary: packing must flush on the fallback's
        // length, not only the primary's.
        let primary = vec![
            RenderedBlock::prose_html("<a href=\"u\">x</a>"),
            RenderedBlock::prose_html("<a href=\"u\">y</a>"),
        ];
        let fallback = vec![
            RenderedBlock::prose("x (a-very-long-destination)"),
            RenderedBlock::prose("y (a-very-long-destination)"),
        ];
        let pairs = split_paired(&primary, &fallback, 40);
        assert!(pairs.iter().all(|(_, f)| f.chars().count() <= 40));
    }

    #[test]
    fn paired_split_empty_twin_gets_no_leading_separator() {
        let primary = vec![
            RenderedBlock::prose_html("<hr>"),
            RenderedBlock::prose_html("<b>b</b>"),
        ];
        let fallback = vec![RenderedBlock::prose(""), RenderedBlock::prose("b")];
        let pairs = split_paired(&primary, &fallback, 100);
        assert_eq!(pairs[0].1, "b");
    }
}
