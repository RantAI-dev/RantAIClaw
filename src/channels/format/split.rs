//! Pack rendered blocks into platform-sized chunks without cutting a code fence
//! or an HTML tag. Prose is packed greedily and hard-split at word boundaries
//! only when a single block exceeds the limit; HTML prose is split only at tag
//! depth 0 so tags stay balanced by construction; code is re-wrapped per chunk
//! so a fence is never orphaned.

use super::{BlockKind, CodeWrap, RenderedBlock};

const SEP: &str = "\n\n";

/// Cap on a fence info string, in chars.
///
/// Every real language identifier is far shorter (`objective-c` is 11,
/// `typescript` 10); 32 clears the whole highlight.js/linguist registry. The cap
/// exists for the budget, not for looks: `wrap_overhead` puts the info string in
/// the fence's FIXED cost, so an uncapped one makes that cost exceed the limit,
/// `budget` saturates to 1, and every chunk overflows (a 300-char info string at
/// limit 64 produced 291 chunks of 309). At 32 the fence costs at most 40 — an
/// order of magnitude under the smallest platform limit (Discord 2000).
const FENCE_LANG_MAX: usize = 32;

/// Width of the backtick fence that can wrap `body`, in backticks.
///
/// A fence must be LONGER than the longest backtick run in its body, or the body
/// closes the block early. The common case is an LLM quoting markdown: it emits a
/// 4-backtick fence around a 3-backtick one, `ast.rs` parses that correctly and
/// hands us a body containing ```` ``` ````, and a hard-coded 3-backtick wrapper
/// then truncates the block at the body's own fence — silently losing the
/// content. Parity guards do not see it: the backtick count stays even.
fn fence_width(body: &str) -> usize {
    let mut longest = 0usize;
    let mut run = 0usize;
    for ch in body.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    (longest + 1).max(3)
}

/// Sanitize a fence info string down to something that cannot break the fence.
///
/// Two rules, for two different failure modes:
/// - First whitespace-delimited word only. Whitespace — a newline above all — in
///   an info string ends the opening fence line early and turns the remainder
///   into content. `ast.rs` already takes the first word, but `wrap_code` is a
///   separate boundary and must emit a valid fence for any block handed to it.
/// - No backtick or tilde. CommonMark forbids backticks in a backtick fence's
///   info string, but a TILDE fence's info string may contain them and survives
///   parsing intact — so an info string of ``a```b`` reaches us and, passed
///   through, emitted three fences: ODD parity, breaking the very invariant
///   `split_respects_small_limit_without_orphan_fence` asserts.
///
/// A delimiter is DROPPED rather than stripped: it makes the fence invalid, and
/// stripping would silently invent a different language (``a```b`` -> `ab`).
/// Over-length is only useless, not invalid, so it truncates instead — which
/// costs a legitimate identifier nothing and keeps the prefix readable.
fn fence_lang(lang: Option<&str>) -> String {
    let word = lang
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if word.contains('`') || word.contains('~') {
        return String::new();
    }
    word.chars().take(FENCE_LANG_MAX).collect()
}

fn wrap_code(body: &str, wrap: &CodeWrap) -> String {
    match wrap {
        CodeWrap::Fence(lang) => {
            let fence = "`".repeat(fence_width(body));
            let mut out = fence.clone();
            out.push_str(&fence_lang(lang.as_deref()));
            out.push('\n');
            out.push_str(body);
            out.push('\n');
            out.push_str(&fence);
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

/// Overhead a wrap adds to `body`, as `(fixed, per_line)`.
///
/// `Fence` and `HtmlPre` pay once around the whole body. `Indent` prefixes four
/// spaces to EVERY line, so it cannot be modelled as a scalar: charging it once
/// lets an N-line chunk overflow the limit by `4 * N` (a 200-line plain table at
/// limit 4096 came out at 4454; single-char lines reached ~3x the limit).
///
/// `Fence` MUST derive from the same [`fence_width`]/[`fence_lang`] helpers
/// `wrap_code` emits from, or the budget lies and the re-wrapped chunk overflows.
/// `wrap_code` writes `<fence><lang>\n<body>\n<fence>`, so the cost around the
/// body is `2 * width + lang + 2` — which reduces to the historical `8 + lang`
/// for a 3-wide fence, the only case that existed before bodies could widen it.
fn wrap_overhead(wrap: &CodeWrap, body: &str) -> (usize, usize) {
    match wrap {
        CodeWrap::Fence(lang) => (
            2 * fence_width(body) + fence_lang(lang.as_deref()).chars().count() + 2,
            0,
        ),
        CodeWrap::HtmlPre => (11, 0),
        CodeWrap::Indent => (0, 4),
    }
}

/// Largest piece a `Fence` can always wrap within `limit`, whatever backticks the
/// piece itself holds.
///
/// [`wrap_overhead`] prices the fence from the WHOLE body, which is the right
/// conservative bound until the body's own backtick run makes that price exceed
/// the limit: `budget` then saturates to 1 and every 1-char piece gets a full
/// fence of its own — a 1000-backtick body at limit 2000 went out as 1000 chunks
/// of 9 chars, i.e. 1000 messages. The run needed is large (> ~limit/2) but it is
/// remote input: a model asked to echo backticks emits exactly this.
///
/// A piece of `b` chars holds a run of at most `b`, so its own fence is at most
/// `b + 1` wide and `wrap_code` writes at most `(b+1) + lang + 1 + b + 1 + (b+1)`
/// = `3b + lang + 4`. Solving `<= limit` gives the bound below, which depends only
/// on the limit — so it cannot saturate. Sound for every `b >= 2`; below that
/// `wrap_code`'s `.max(3)` floor applies instead and costs `b + lang + 8`, which
/// fits whenever this bound is >= 2 (i.e. `limit >= lang + 10`) — and where it is
/// not, `budget` was already clamped to 1 and nothing changes.
///
/// `split_oversized` takes the MAX of this and `limit - fixed`. Both are proved
/// for every piece up to their own value, so the larger is safe for every piece
/// up to it.
fn fence_floor(wrap: &CodeWrap, limit: usize) -> usize {
    match wrap {
        CodeWrap::Fence(lang) => {
            let lang = fence_lang(lang.as_deref()).chars().count();
            limit.saturating_sub(lang + 4) / 3
        }
        // Neither wrap's cost depends on the piece, so `limit - fixed` never
        // saturates for them and there is no floor to raise.
        CodeWrap::HtmlPre | CodeWrap::Indent => 0,
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
            // Budget from the WHOLE body, then re-wrap each piece: `wrap_code`
            // recomputes `fence_width(piece)`, which can only be <= the width
            // charged here. Splitting never merges two backtick runs — it cuts
            // between lines/words/chars and rejoins with the separator it cut on
            // — so a piece's longest run cannot exceed the body's. A narrower
            // fence than budgeted just under-fills the chunk, never overflows it.
            let (fixed, per_line) = wrap_overhead(&wrap, &block.text);
            let budget = limit
                .saturating_sub(fixed)
                .max(fence_floor(&wrap, limit))
                .max(1);
            pack_lines(&block.text, budget, per_line)
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

/// Greedily pack whole lines so that the WRAPPED chunk fits `budget` chars.
///
/// `per_line` is what the wrap will add to each line (see [`wrap_overhead`]); it
/// must be charged as each line is packed, not once per chunk. `cost` tracks the
/// wrapped size of `cur`, so the caller's `wrap_code` can never push a chunk past
/// the limit.
fn pack_lines(text: &str, budget: usize, per_line: usize) -> Vec<String> {
    let budget = budget.max(1);
    // Budget for a piece that will stand alone as its own single-line chunk.
    let line_budget = budget.saturating_sub(per_line).max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cost = 0usize;
    for line in text.lines() {
        let line_len = line.chars().count();
        // `+1` for the '\n' that would join this line to the previous one. `cur`
        // is non-empty here, so that join is real.
        if !cur.is_empty() && cost + line_len + per_line + 1 > budget {
            out.push(std::mem::take(&mut cur));
            cost = 0;
        }
        if line_len + per_line > budget {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cost = 0;
            }
            out.extend(hard_split(line, line_budget));
        } else {
            // Read the join AFTER the flush above, never before it: a flush
            // empties `cur` and writes no '\n', so a join priced against the
            // pre-flush `cur` charges a newline that was never written. `cost`
            // then runs 1 over for the rest of the chunk and closes it a char
            // early — safe, but it under-fills every chunk after a flush.
            let join = usize::from(!cur.is_empty());
            if join == 1 {
                cur.push('\n');
            }
            cur.push_str(line);
            cost += line_len + per_line + join;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// How far back a char-boundary cut will look for the `&` of an entity, in chars.
///
/// The escaper (`html::escape_html`) emits only `&amp; &lt; &gt; &quot;
/// &#39;` — 6 chars at worst — so 12 is double the longest sequence that can
/// actually reach here. The bound is what makes a STRAY `&` safe: text is not
/// required to be escaped (`hard_split` also splits Plain prose and code bodies,
/// where `&` is literal), so `cats&dogs` must not drag the cut arbitrarily far
/// back looking for a `;` that does not exist. It also keeps the scan O(1) per
/// cut rather than O(limit).
const ENTITY_SCAN_MAX: usize = 12;

/// Move a char-boundary cut at `end` back so it never lands inside an `&…;`.
///
/// Scans back from the cut for the nearest entity-significant char and decides:
/// a `&` means an entity opened before the cut and did not close before it, so
/// the cut lands INSIDE it — move the cut to before the `&`. A `;` means the cut
/// follows a complete entity. Anything an entity body cannot contain means the
/// cut is not in one. All three are safe as-is.
///
/// Returns a cut STRICTLY greater than `start` in every case, which is what keeps
/// `hard_split`'s loop terminating: a cut backed all the way up to `start` would
/// make the loop re-cut the same range forever (the same failure the `limit.max(1)`
/// clamp guards). When backing up cannot make progress — `start` itself is the
/// `&`, reachable only when `limit` is shorter than the entity — the unadjusted
/// cut is kept and the entity is split, which is unavoidable at that limit and
/// never happens at a real platform limit, where any entity fits.
fn entity_safe_cut(chars: &[char], start: usize, end: usize) -> usize {
    let lower = start.max(end.saturating_sub(ENTITY_SCAN_MAX));
    for j in (lower..end).rev() {
        match chars[j] {
            '&' => return if j > start { j } else { end },
            ';' => return end,
            c if c.is_ascii_alphanumeric() || c == '#' => {}
            _ => return end,
        }
    }
    end
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
            // Cut by index rather than `take(limit)` so the cut can be moved
            // back off an entity. Every cut still advances (see
            // `entity_safe_cut`), so this terminates for any `limit >= 1`.
            let chars: Vec<char> = word.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                let mut end = (i + limit).min(chars.len());
                if end < chars.len() {
                    end = entity_safe_cut(&chars, i, end);
                }
                out.push(chars[i..end].iter().collect::<String>());
                i = end;
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
/// `pending` is flushed at every depth-0 boundary: a space/newline, and — the
/// part that makes the oversized branch below correct — the START and END of
/// every top-level element. That yields the invariant every flush relies on:
///
/// > `pending` holds EITHER tag-free text OR exactly one complete element.
///
/// It holds because a `<` at depth 0 flushes before the tag is appended (so the
/// text before it leaves alone), and depth returning to 0 flushes right after the
/// element closes (so the element leaves alone). Text accumulated at depth 0 can
/// therefore never contain a `<`, since one would have flushed it.
///
/// Without those two flush points, `pending` mixed text and elements, the
/// oversized branch saw `contains('<')` and emitted the ENTIRE run — so one
/// `<b>` in a space-free 5709-char CJK paragraph turned `[4096, 1604]` into a
/// single 5709-char chunk Telegram rejects, while the same text rendered Plain
/// split fine. The exemption is only sound for something that truly cannot be
/// broken.
///
/// A single element longer than `limit` IS atomic — it cannot be split without
/// breaking its tags — so it alone is emitted oversized. See
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
                // Per the flush invariant, a `<` here means `pending` is exactly
                // ONE element and nothing else, so it is genuinely atomic:
                // splitting it would unbalance its tags. It goes out oversized.
                out.push(std::mem::take(pending));
            } else {
                // Tag-free text between elements. Per the flush invariant it is
                // ONE word plus at most its trailing terminator — any earlier
                // space or newline at depth 0 would already have flushed it — so
                // `hard_split`'s word loop has nothing to break on here and it is
                // the char-level cut, with its entity back-off, that does the
                // work. That is exactly what an over-limit word needs; do not
                // read this call as word-wrapping.
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
            // A top-level element opens here. Flush the text before it on its
            // own: carried into `pending` alongside the element, it would make
            // the whole run look atomic. Depth is 0, so this cannot cut inside
            // an element.
            if depth == 0 {
                flush(&mut pending, &mut cur, &mut out);
            }
            let closing = chars.peek() == Some(&'/');
            let mut tag = String::from('<');
            for c in chars.by_ref() {
                tag.push(c);
                if c == '>' {
                    break;
                }
            }
            // A PREFIX test, deliberately. It must accept `<br>`, `<br/>`,
            // `<br />` and any attribute a renderer might add, and matching the
            // name exactly would need a name parser here — in the depth tracker,
            // to buy nothing: `br` and `hr` are the only HTML tags that start
            // with these prefixes, so the only false positive is a tag no
            // renderer emits (the set is `<b> <i> <s> <code> <pre> <a> <h1>..<h6>
            // <blockquote> <ul> <ol> <li> <p> <br/> <hr>`).
            //
            // The asymmetry is why it stays: a void tag misread as an OPENER
            // never returns depth to 0, so the rest of the message becomes one
            // atomic `pending` and goes out oversized — the bug this function has
            // already been burned by. A hypothetical `<brie>` misread as void
            // costs a balanced pair, and nothing emits one.
            let void = tag.ends_with("/>") || tag.starts_with("<br") || tag.starts_with("<hr");
            if closing {
                depth = (depth - 1).max(0);
            } else if !void {
                depth += 1;
            }
            pending.push_str(&tag);
            // Back to depth 0: the element (or void/stray tag) is complete and
            // `pending` is exactly that one element. Flush it as the single
            // atomic unit it is, so trailing text never rides along with it.
            if depth == 0 {
                flush(&mut pending, &mut cur, &mut out);
            }
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
    fn fence_widens_past_backticks_in_the_body() {
        let blocks = vec![RenderedBlock::code(
            "```\ncode\n```",
            CodeWrap::Fence(Some("md".into())),
        )];
        let chunks = split(&blocks, 100);
        assert_eq!(chunks, vec!["````md\n```\ncode\n```\n````".to_string()]);

        let reparsed = crate::channels::format::ast::parse(&chunks[0]);
        assert_eq!(reparsed.len(), 1, "{reparsed:?}");
        match &reparsed[0] {
            crate::channels::format::ast::Block::CodeBlock { lang, code } => {
                assert_eq!(lang.as_deref(), Some("md"));
                assert_eq!(code, "```\ncode\n```\n");
            }
            other => panic!("expected one code block, got {other:?}"),
        }
    }

    #[test]
    fn fence_info_string_with_backticks_keeps_fence_parity_even() {
        let blocks = vec![RenderedBlock::code(
            "code",
            CodeWrap::Fence(Some("a```b".into())),
        )];
        let chunks = split(&blocks, 100);
        for c in &chunks {
            assert_eq!(c.matches("```").count() % 2, 0, "odd fence parity: {c}");
        }
        assert_eq!(chunks, vec!["```\ncode\n```".to_string()]);
    }

    #[test]
    fn long_fence_info_string_does_not_overflow_every_chunk() {
        let code = (0..50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let blocks = vec![RenderedBlock::code(
            code,
            CodeWrap::Fence(Some("z".repeat(300))),
        )];
        let chunks = split(&blocks, 64);
        assert!(
            chunks.iter().all(|c| c.chars().count() <= 64),
            "chunk lengths {:?}",
            chunks.iter().map(|c| c.chars().count()).collect::<Vec<_>>()
        );
    }

    // `wrap_overhead` must charge the fence width the BODY forces, not a
    // hard-coded 3-backtick fence. The bug needs two things at once: a body long
    // enough to split, AND a wide backtick run sitting in a chunk that packing
    // has filled to the budget. None of the three fence tests above has both —
    // their bodies are 4 and 12 chars and never split at all, and the long-info
    // -string case only over-charges, which is the safe direction. So all three
    // pass against the historical `8 + lang` formula while a real chunk overflows:
    // at limit 2000 the widest chunk came out 2005, at 4096 it came out 4106 —
    // both rejected by the platform.
    #[test]
    fn wide_fence_in_an_oversized_body_respects_limit() {
        // SHORT lines on purpose: packing is line-granular, so long lines leave
        // the last one short of the budget and the 16-char lie fits in the slack
        // (48-char lines hide it entirely at limit 2000). Short lines pack tight
        // against the budget, which is where the miscount actually shows.
        let mut lines: Vec<String> = (0..800).map(|_| "c".repeat(8)).collect();
        // A 10-backtick run mid-body forces an 11-wide fence, so the real wrap
        // costs 24 chars where the old formula charged 8.
        lines[400] = "`".repeat(10);
        let code = lines.join("\n");
        let blocks = vec![RenderedBlock::code(code, CodeWrap::Fence(None))];
        for limit in [2000usize, 4096] {
            let chunks = split(&blocks, limit);
            assert!(chunks.len() > 1, "limit {limit}: body must actually split");
            assert!(
                chunks.iter().all(|c| c.chars().count() <= limit),
                "limit {limit}: chunk lengths {:?}",
                chunks.iter().map(|c| c.chars().count()).collect::<Vec<_>>()
            );
        }
    }

    // A backtick run wide enough to force `fixed > limit` saturates `budget` to 1,
    // and a 1-char piece per chunk turns one message into a message PER CHAR:
    // 1000 backticks at limit 2000 sent 1000 chunks. `fence_floor` is what stops
    // it — a piece is bounded by what its OWN fence can wrap, not by what the
    // whole body's fence costs.
    //
    // Both `lang` arms matter: the info string is charged in the same budget, so
    // an unshared `lang` term is an off-by-`lang` overflow. At limit 2000 the
    // widest chunk lands on 1999 (no lang) and exactly 2000 (lang `rust`) — the
    // bound is tight, so an arithmetic slip shows up as a real overflow here.
    #[test]
    fn a_wide_backtick_run_does_not_explode_into_a_chunk_per_char() {
        let limit = 2000;
        for lang in [None, Some("rust".to_string())] {
            let blocks = vec![RenderedBlock::code(
                "`".repeat(1000),
                CodeWrap::Fence(lang.clone()),
            )];
            let chunks = split(&blocks, limit);
            let lens: Vec<usize> = chunks.iter().map(|c| c.chars().count()).collect();
            assert!(
                chunks.iter().all(|c| c.chars().count() <= limit),
                "lang {lang:?}: chunk lengths {lens:?}"
            );
            assert!(
                chunks.len() <= 4,
                "lang {lang:?}: {} chunks for a 1000-char body, lengths {lens:?}",
                chunks.len()
            );
        }
    }

    // `Indent` adds 4 chars to EVERY line. Charging that overhead once per chunk
    // (instead of per line) lets an N-line chunk overrun the limit by 4*N — the
    // Fence path has always been covered, this path was not.
    #[test]
    fn oversized_indented_code_respects_limit() {
        let code = (0..50)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let blocks = vec![RenderedBlock::code(code, CodeWrap::Indent)];
        assert!(split(&blocks, 40).iter().all(|c| c.chars().count() <= 40));
    }

    // Regression for the realistic shape: a long plain-rendered ASCII table.
    // Many short lines maximize the per-line overhead the old scalar model lost.
    #[test]
    fn oversized_indented_many_short_lines_respects_limit() {
        let code = (0..500).map(|_| "x").collect::<Vec<_>>().join("\n");
        let blocks = vec![RenderedBlock::code(code, CodeWrap::Indent)];
        assert!(split(&blocks, 100).iter().all(|c| c.chars().count() <= 100));
    }

    // `line_cost` charges `+1` for the '\n' that joins this line to the previous
    // one. Read before the flush block, that `+1` is stale: the flush empties
    // `cur`, so no '\n' is written, yet the phantom join is still charged. Every
    // chunk after a flush then believes it is one char fuller than it is.
    //
    // The input is tuned so the lie actually changes the packing: at budget 10
    // "cccc" (4) plus its join is the exact char that "eeeee" (5) needs, so the
    // stale `+1` closes the chunk one char early and "eeeee" becomes a third
    // piece. A body that does not pack tight against the budget hides this
    // entirely — under-filling by 1 is invisible unless something needs that 1.
    #[test]
    fn pack_lines_fills_the_budget_after_a_flush() {
        let pieces = pack_lines("aaaa\nbbbb\ncccc\neeeee", 10, 0);
        assert_eq!(
            pieces,
            vec!["aaaa\nbbbb".to_string(), "cccc\neeeee".to_string()],
            "a chunk after a flush must be packed to the full budget"
        );
        assert_eq!(
            pieces[1].chars().count(),
            10,
            "post-flush chunk short of budget"
        );
        // Packing only inserts break points; it never drops content.
        assert_eq!(pieces.join("\n"), "aaaa\nbbbb\ncccc\neeeee");
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

        // ...and ONLY the atomic element may exceed the limit. The exemption is
        // "an element cannot be broken", not "anything adjacent to an element
        // rides along": the ` tail` next to it has a legal break point and must
        // use it. Without this, an oversized chunk may quietly carry unbounded
        // breakable text (see `html_prose_unspaced_run_splits_with_and_without_
        // an_element`, where that text is 6000 chars).
        let oversized: Vec<&String> = chunks.iter().filter(|c| c.chars().count() > 50).collect();
        assert_eq!(oversized.len(), 1, "chunks {chunks:?}");
        assert_eq!(
            *oversized[0],
            format!("<b>{}</b>", "y".repeat(80)),
            "the oversized chunk must be the bare element, with no text dragged along"
        );
    }

    // Two elements that each fit the limit EXACTLY, separated by one space. The
    // single space is the whole bug: carried into `pending` behind the element it
    // made the pair look like one atomic run of limit+1, which then went out
    // oversized — `[4097, 4096]` at Telegram's real limit, from input that is
    // entirely legal and needs no exemption at all. Nothing here is unsplittable.
    #[test]
    fn html_prose_adjacent_fitting_elements_respect_limit() {
        let limit = 4096;
        let element = format!("<b>{}</b>", "a".repeat(4089));
        assert_eq!(element.chars().count(), limit, "element must fit exactly");
        let chunks = split(
            &[RenderedBlock::prose_html(format!("{element} {element}"))],
            limit,
        );
        assert!(
            chunks.iter().all(|c| c.chars().count() <= limit),
            "two limit-sized elements must not merge into an over-limit chunk: {:?}",
            chunks.iter().map(|c| c.chars().count()).collect::<Vec<_>>()
        );
        for chunk in &chunks {
            assert_eq!(chunk.matches("<b>").count(), chunk.matches("</b>").count());
        }
    }

    // The decisive control pair for the depth-0 break: the SAME space-free run,
    // once alone and once with one small element in front. Only the element
    // differs, so a split/no-split divergence can only come from the element
    // handling. This is the ASCII analogue of the CJK repro — Chinese/Japanese/
    // Thai prose carries no spaces, so one `<b>` used to flip a 5709-char
    // paragraph from `[4096, 1604]` to a single undeliverable 5709-char chunk.
    // Base64 blobs, minified JSON and stack traces after inline markup are the
    // ASCII shapes that hit the same path.
    #[test]
    fn html_prose_unspaced_run_splits_with_and_without_an_element() {
        let run = "x".repeat(6000);
        let limit = 4096;

        let bare = split(&[RenderedBlock::prose_html(run.clone())], limit);
        assert!(
            bare.iter().all(|c| c.chars().count() <= limit),
            "control: a space-free run with no element must split: {:?}",
            bare.iter().map(|c| c.chars().count()).collect::<Vec<_>>()
        );

        let with_element = split(
            &[RenderedBlock::prose_html(format!("<b>x</b>{run}"))],
            limit,
        );
        assert!(
            with_element.iter().all(|c| c.chars().count() <= limit),
            "one small element must not make the same run unsplittable: {:?}",
            with_element
                .iter()
                .map(|c| c.chars().count())
                .collect::<Vec<_>>()
        );
        for chunk in &with_element {
            assert_eq!(
                chunk.matches("<b>").count(),
                chunk.matches("</b>").count(),
                "unbalanced: {chunk:?}"
            );
        }

        // The mirror, and NOT a duplicate of the case above: text-then-element is
        // the shape the CJK repro actually had, and it leans on a different flush
        // point. `<b>x</b>{run}` is saved by the flush AFTER depth returns to 0 —
        // the element leaves `pending` before the run accumulates. Only
        // `{run}<b>x</b>` exercises the flush BEFORE the tag is consumed; without
        // it the run rides into `pending` alongside the element, `contains('<')`
        // then reads the whole thing as one atomic element, and the 6008 chars go
        // out as a single chunk Telegram rejects.
        let element_last = split(
            &[RenderedBlock::prose_html(format!("{run}<b>x</b>"))],
            limit,
        );
        assert!(
            element_last.iter().all(|c| c.chars().count() <= limit),
            "an element AFTER the run must not make it unsplittable: {:?}",
            element_last
                .iter()
                .map(|c| c.chars().count())
                .collect::<Vec<_>>()
        );
        for chunk in &element_last {
            assert_eq!(
                chunk.matches("<b>").count(),
                chunk.matches("</b>").count(),
                "unbalanced: {chunk:?}"
            );
        }
    }

    // Every `</b><code>` seam is a clean depth-0 break. Emitting the whole run
    // because it merely CONTAINS tags ignores all of them: 600 repeats produced
    // one 13200-char chunk at limit 20.
    #[test]
    fn html_prose_breaks_at_element_seams() {
        let text = "<b>a</b><code>b</code>".repeat(600);
        let limit = 20;
        let chunks = split(&[RenderedBlock::prose_html(text)], limit);
        assert!(
            chunks.iter().all(|c| c.chars().count() <= limit),
            "every element fits the limit, so every chunk must: {:?}",
            chunks.iter().map(|c| c.chars().count()).collect::<Vec<_>>()
        );
        for chunk in &chunks {
            assert_eq!(chunk.matches("<b>").count(), chunk.matches("</b>").count());
            assert_eq!(
                chunk.matches("<code>").count(),
                chunk.matches("</code>").count()
            );
        }
    }

    // `hard_split`'s char-level fallback is the last resort for a word longer
    // than the limit. It must not cut inside an `&…;`: the user sees literal
    // `&`/`amp;` garbage and the original character is destroyed. Reachable at
    // Telegram's real 4096 limit, not just at synthetic ones.
    #[test]
    fn hard_split_never_cuts_an_html_entity() {
        // What the escaper emits for `"a&b".repeat(3000)` — one 21000-char
        // space-free word, so the char-level fallback is what splits it.
        let text = "a&amp;b".repeat(3000);
        let limit = 4096;
        let chunks = split(&[RenderedBlock::prose_html(text)], limit);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= limit));

        // The only `&` and `;` in this input come from `&amp;`, so these two
        // assertions can actually fail: any `&` must reach its `;` inside the
        // same chunk, and no chunk may open with an orphaned entity tail.
        for chunk in &chunks {
            if let Some(amp) = chunk.rfind('&') {
                assert!(
                    chunk[amp..].contains(';'),
                    "chunk ends inside an entity: {:?}",
                    &chunk[amp..]
                );
            }
            match (chunk.find('&'), chunk.find(';')) {
                (Some(amp), Some(semi)) => assert!(
                    semi > amp,
                    "chunk starts with an entity tail: {:?}",
                    &chunk[..=semi]
                ),
                (None, Some(semi)) => {
                    panic!("chunk starts with an entity tail: {:?}", &chunk[..=semi])
                }
                _ => {}
            }
        }
        // Content is preserved exactly: splitting only removes break points.
        assert_eq!(chunks.concat(), "a&amp;b".repeat(3000));
    }

    // `hard_split_html` reproduces its input EXACTLY: it inserts chunk
    // boundaries and drops nothing, spaces included. Chunks therefore keep a
    // trailing space (`"<i>aaaa</i> "`), which reads like a cosmetic slip and is
    // not one — that space is the word separator between two elements, and the
    // break merely fell on it. Trimming chunks would look tidier in isolation
    // and would silently make this invariant false; `hard_split`'s entity test
    // cannot catch that, as its input has no spaces to lose.
    //
    // Trimming the other end is worse: a chunk can legitimately OPEN with
    // meaningful whitespace, e.g. a Telegram list's indent column
    // (`"1. First item here\n\n   "` splits right there).
    #[test]
    fn html_prose_split_preserves_its_input_exactly() {
        let text = "<i>aaaa</i> <i>bbbb</i> <i>cccc</i>";
        let chunks = split(&[RenderedBlock::prose_html(text)], 12);
        assert!(chunks.len() > 1, "input must actually split: {chunks:?}");
        assert_eq!(chunks.concat(), text, "chunks {chunks:?}");
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

    // NOT "counts are equal" — `Vec<(String, String)>` makes that true by
    // construction and no code could falsify it. What is worth pinning is that
    // every pair actually carries BOTH members: a fitting block must never leave
    // a twin empty, because callers read an empty fallback as "no sound twin
    // exists" (see `paired_split_oversized_block_has_no_twin`) and bail.
    #[test]
    fn paired_split_gives_both_members_on_every_pair() {
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
