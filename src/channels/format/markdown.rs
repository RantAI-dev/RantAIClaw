//! StdMarkdown renderer: keep CommonMark-ish markup (`**bold**`, `#` headings,
//! `[t](u)`, fenced code). Tables stay native (`| a | b |`) for platforms that
//! render them (Mattermost) or become ASCII in a fenced block for those that do
//! not (Discord, DingTalk).
//!
//! Because the output is markdown, `Inline::Text` cannot be emitted raw:
//! `pulldown-cmark` STRIPS backslash escapes while parsing, so `\*literal\*`
//! reaches this renderer as plain `Inline::Text("*literal*")`, indistinguishable
//! from a `*literal*` the user never escaped. Emitted raw it hands the
//! platform's own parser emphasis the user explicitly asked not to have. See
//! [`push_text`] for what is escaped and — just as deliberately — what is not.

use super::ast::{Block, Inline, TableAlign};
use super::nest::{indent_continuation, prefix_lines};
use super::split::join_all;
use super::table::ascii_table;
use super::{CodeWrap, RenderedBlock};

/// Where an inline run is being written. The two contexts disarm different
/// things: prose can start a block (`# `, `- `, `> `) and a table cell cannot,
/// but a cell's `|` is the column delimiter where a paragraph's is just a char.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Prose,
    Cell,
}

/// Whether the next char written to `out` lands where a block marker is
/// recognised: the start of a line, or after only the <= 3 leading spaces
/// CommonMark still allows before one.
///
/// `out` already holds everything emitted ahead of that char — a `## ` marker,
/// a `**` opener, earlier text — so it is the entire context this needs. List
/// bullets and `> ` quote prefixes are added by the callers *after* rendering,
/// which is why a paragraph nested in either still reports a block start here:
/// it genuinely is one once the prefix lands.
fn at_block_start(out: &str) -> bool {
    let line = out.rsplit('\n').next().unwrap_or("");
    line.len() <= 3 && line.bytes().all(|b| b == b' ')
}

/// Length of the run of identical chars starting at `i`.
fn run_len(chars: &[char], i: usize) -> usize {
    let c = chars[i];
    chars[i..].iter().take_while(|&&x| x == c).count()
}

/// Whether a run of `c` sitting at a block start opens a block-level construct.
///
/// These markers are LINE-ANCHORED, and that is what keeps the escaping cheap:
/// escaping the run's FIRST char disarms the whole run, because `\### x` no
/// longer begins the line with a `#`. Contrast [`opens_inline_span`].
fn starts_block(c: char, run: usize, after: Option<char>) -> bool {
    let spaced = after.is_none_or(|a| a == ' ' || a == '\t');
    match c {
        // ATX heading: 1-6 hashes then a space or end of line. `#hashtag` is not
        // a heading and neither is `####### x`, so neither is escaped.
        '#' => run <= 6 && spaced,
        '>' => true,
        // A bullet needs the trailing space; 3+ make a thematic break.
        '-' | '+' | '*' => spaced || run >= 3,
        // `_ x` is not a bullet, so only the thematic break applies.
        '_' => run >= 3,
        _ => false,
    }
}

/// Whether the `.`/`)` about to be written closes an ordered-list marker: it
/// sits on a line holding nothing but its 1-9 leading digits.
///
/// The digits are inert on their own — only the delimiter arms the marker — so
/// only the delimiter is escaped, giving `1\. not a list` rather than a
/// backslash in front of the number.
///
/// This looks BACKWARD at `out` rather than forward from the digits, because
/// the two need not arrive in the same node: only `Text` is coalesced, so the
/// digits can still reach `out` as another inline entirely (`` `1` ``, `*1*`)
/// with the delimiter opening this node. `out` holds them either way, which a
/// scan starting at the digits within `text` would not.
fn closes_ordered_marker(out: &str, next: Option<char>) -> bool {
    if !next.is_none_or(|n| n == ' ' || n == '\t') {
        return false;
    }
    let line = out.rsplit('\n').next().unwrap_or("");
    let digits = line.trim_start_matches(' ');
    // CommonMark: <= 3 spaces of indent, then 1-9 digits, then the delimiter.
    line.len() - digits.len() <= 3
        && !digits.is_empty()
        && digits.len() <= 9
        && digits.bytes().all(|b| b.is_ascii_digit())
}

/// Whether a `*`/`_`/`~` run can open or close an inline span where it stands.
///
/// Inline spans are NOT line-anchored, so — unlike a block marker — every char
/// of the run must be escaped: `\**bold**` still emphasises, only
/// `\*\*bold\*\*` is inert.
///
/// The flanking test is what keeps ordinary prose readable. A delimiter run
/// touching whitespace on BOTH sides can neither open nor close a span
/// (CommonMark's left/right-flanking rule), so `Cost: $5 * 3` needs no
/// backslash — and inserting one there would be a worse read than the bug this
/// escaping exists to fix.
fn opens_inline_span(c: char, run: usize, before: Option<char>, after: Option<char>) -> bool {
    let space = |x: Option<char>| x.is_none_or(char::is_whitespace);
    let flanking = !space(before) || !space(after);
    match c {
        '*' => flanking,
        // CommonMark forbids INTRAWORD `_` emphasis, so `snake_case` and
        // `run_time_ms` are inert and stay bare.
        '_' => {
            flanking
                && !(before.is_some_and(char::is_alphanumeric)
                    && after.is_some_and(char::is_alphanumeric))
        }
        // A lone `~` is left bare DELIBERATELY — not because it is inert. This
        // parser strikes on a single tilde (`parse("~a~")` -> `Strikethrough`),
        // so `\~a\~` really does come back struck with its tildes gone. That is
        // the accepted trade-off, not an oversight: `~/path` and `(~2s)` are
        // ordinary prose, the shipped targets (Discord, Mattermost) only strike
        // on `~~`, and a backslash in front of every stray tilde reads worse than
        // the loss on a parser we do not send to.
        //
        // `run >= 2` is exactly what buys that trade-off, and it is load-bearing:
        // dropping it escapes the lone tilde in `(~2s)` too. Both
        // `escaped_lone_tildes_are_not_escaped` and
        // `ordinary_prose_is_not_over_escaped` fail if it goes.
        //
        // At `run >= 2` and up, two live constructs are being disarmed: `~~`
        // strikes, and 3+ tildes at a block start open a FENCE — emitted bare,
        // `~~~~a` is not merely struck, it swallows the paragraph into an empty
        // code block. See `escaped_tilde_run_does_not_reopen_a_code_fence`.
        //
        // This rule can only see those runs because `ast.rs`'s `push_inline`
        // COALESCES adjacent `Text`. `run`/`after` are computed within one node,
        // and pulldown splits a flanking run at every stripped escape, so
        // `\~\~a\~\~` arrives as `["~", "~a", "~", "~"]` — four run=1 nodes. Left
        // split, no run here ever reaches 2 and the fully-escaped spellings go
        // back out as LIVE `~~a~~`/`~~~~a`. Do not simplify that coalescing away.
        // See `fully_escaped_strikethrough_survives_a_round_trip_as_text`.
        '~' => run >= 2 && flanking,
        _ => false,
    }
}

/// Whether `c` must be escaped wherever it stands, independent of run/position.
fn escape_anywhere(c: char, next: Option<char>, ctx: Ctx) -> bool {
    match c {
        // The column delimiter. GFM strips `\|` from a row BEFORE inline
        // parsing, so this is the only way to carry a pipe into a cell — and
        // the whole reason a cell needs a context of its own.
        '|' => ctx == Ctx::Cell,
        // Neither has an inert position, so neither gets a position rule: a
        // backtick opens a code span that any LATER backtick closes, and a `[`
        // opens a link/image/reference. (A bare `]` opens nothing on its own,
        // so it is left alone.) One arm: `match_same_arms` (pedantic) rejects
        // splitting identical bodies in two.
        '`' | '[' => true,
        // A backslash only needs doubling where it would swallow the next char
        // as an escape: before ASCII punctuation (all CommonMark lets a
        // backslash escape), or at this node's end, where the next inline's
        // first char is not visible from here. `C:\Users\bin` stays bare.
        '\\' => next.is_none_or(|n| n.is_ascii_punctuation()),
        _ => false,
    }
}

/// Write `text`, escaping the metacharacters that would be RE-READ as markup
/// where they stand — and deliberately no others.
///
/// Over-escaping is a real cost, not a safe default: every stray backslash is
/// visible to the reader on any platform that does not honour the escape, and
/// `Cost: $5 \* 3` is a worse message than the bug. So each rule is scoped to
/// the position where the character is actually live (see the helpers above).
fn push_text(out: &mut String, text: &str, ctx: Ctx) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let run = run_len(&chars, i);
        let after = chars.get(i + run).copied();
        // Everything ahead of this char is already in `out`, so `out` answers
        // both "is this a block start?" and "what is the preceding char?".
        let before = if i == 0 {
            out.chars().next_back()
        } else {
            Some(chars[i - 1])
        };

        // Block markers: line-anchored, so the run's first char is enough.
        if ctx == Ctx::Prose {
            // An ordered marker is judged from the digits already in `out`, so
            // this is NOT under `at_block_start` — by the time the delimiter
            // arrives, the digits have moved the line off its start.
            if matches!(c, '.' | ')') && closes_ordered_marker(out, chars.get(i + 1).copied()) {
                out.push('\\');
                out.push(c);
                i += 1;
                continue;
            }
            if at_block_start(out) && starts_block(c, run, after) {
                out.push('\\');
                out.push(c);
                i += 1;
                continue;
            }
        }

        // Inline spans: not line-anchored, so the whole run must go.
        if opens_inline_span(c, run, before, after) {
            for _ in 0..run {
                out.push('\\');
                out.push(c);
            }
            i += run;
            continue;
        }

        if escape_anywhere(c, chars.get(i + 1).copied(), ctx) {
            out.push('\\');
        }
        out.push(c);
        i += 1;
    }
}

fn inlines_md(inlines: &[Inline], ctx: Ctx) -> String {
    let mut out = String::new();
    push_inlines(&mut out, inlines, ctx);
    out
}

/// Append `inlines` to `out`.
///
/// This appends rather than returning a fresh `String` so that `out`'s tail —
/// a `## ` heading marker, a `**` opener, the previous word — stays visible to
/// [`push_text`], which needs it to tell a block start from mid-line and to
/// flank an emphasis run.
fn push_inlines(out: &mut String, inlines: &[Inline], ctx: Ctx) {
    for inline in inlines {
        match inline {
            Inline::Text(t) => push_text(out, t, ctx),
            Inline::Code(t) => {
                out.push('`');
                // A code span takes NO backslash escapes — a `\` is literal
                // inside one. The lone exception is a table cell's `|`: GFM
                // strips the backslash from a row before inline parsing, which
                // makes `\|` the only way to get a pipe into a cell's code span.
                match ctx {
                    Ctx::Cell => out.push_str(&t.replace('|', "\\|")),
                    Ctx::Prose => out.push_str(t),
                }
                out.push('`');
            }
            Inline::Strong(c) => {
                out.push_str("**");
                push_inlines(out, c, ctx);
                out.push_str("**");
            }
            Inline::Emphasis(c) => {
                out.push('*');
                push_inlines(out, c, ctx);
                out.push('*');
            }
            Inline::Strikethrough(c) => {
                out.push_str("~~");
                push_inlines(out, c, ctx);
                out.push_str("~~");
            }
            Inline::Link { text, url } => {
                out.push('[');
                push_inlines(out, text, ctx);
                out.push_str("](");
                // Same reasoning as the code span: a destination is not inline
                // markup, but a raw `|` would still end the cell early.
                match ctx {
                    Ctx::Cell => out.push_str(&url.replace('|', "\\|")),
                    Ctx::Prose => out.push_str(url),
                }
                out.push(')');
            }
            Inline::SoftBreak => out.push('\n'),
            Inline::HardBreak => out.push_str("  \n"),
        }
    }
}

fn native_table(
    headers: &[Vec<Inline>],
    rows: &[Vec<Vec<Inline>>],
    align: &[TableAlign],
) -> String {
    let head = headers
        .iter()
        .map(|c| inlines_md(c, Ctx::Cell))
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
            .map(|c| inlines_md(c, Ctx::Cell))
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
            // Append onto the marker, so the heading text is correctly seen as
            // mid-line rather than as a fresh block start.
            push_inlines(&mut text, inlines, Ctx::Prose);
            RenderedBlock::prose(text)
        }
        Block::Paragraph(inlines) => RenderedBlock::prose(inlines_md(inlines, Ctx::Prose)),
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
    use crate::channels::format::table::inline_plain;

    fn md(src: &str, native: bool) -> String {
        render(&parse(src), native)
            .iter()
            .map(|b| b.text.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The single paragraph `src` renders to, as re-parsed by a markdown reader.
    ///
    /// The renderer's contract is not "the output string looks right" but "the
    /// PLATFORM's parser sees what the user wrote", so every escaping test here
    /// asserts on the re-parsed tree rather than on the emitted text.
    fn round_trip_paragraph(src: &str) -> Vec<Inline> {
        let out = md(src, false);
        let blocks = parse(&out);
        match blocks.as_slice() {
            [Block::Paragraph(inlines)] => inlines.clone(),
            other => panic!("{src:?} rendered to {out:?}, which re-parses as {other:?}"),
        }
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

    // --- escaping -----------------------------------------------------------
    //
    // pulldown-cmark STRIPS backslash escapes while parsing: `\*` arrives as
    // `Inline::Text("*")`, indistinguishable from a `*` the user typed bare.
    // Re-emitting that text raw hands the platform's own parser markup the user
    // never wrote. Each test below asserts on the RE-PARSED tree, because that
    // is what the reader's client actually does with our output.

    #[test]
    fn escaped_emphasis_survives_a_round_trip_as_text() {
        let inlines = round_trip_paragraph(r"\*not emphasis\*");
        assert!(
            !inlines
                .iter()
                .any(|i| matches!(i, Inline::Emphasis(_) | Inline::Strong(_))),
            "text became emphasis: {inlines:?}"
        );
        assert_eq!(inline_plain(&inlines), "*not emphasis*");
    }

    #[test]
    fn escaped_hash_stays_a_paragraph_after_a_round_trip() {
        // `round_trip_paragraph` panics if this re-parses as a Heading.
        let inlines = round_trip_paragraph(r"\# not heading");
        assert_eq!(inline_plain(&inlines), "# not heading");
    }

    #[test]
    fn escaped_ordered_marker_stays_a_paragraph_after_a_round_trip() {
        // `round_trip_paragraph` panics if this re-parses as a List.
        let inlines = round_trip_paragraph(r"1\. not a list");
        assert_eq!(inline_plain(&inlines), "1. not a list");
    }

    // `|` is the cell delimiter in a native table, so an unescaped one splits a
    // row into more cells than the header has. GFM then TRUNCATES the excess
    // rather than growing the table — so a cell COUNT alone passes vacuously
    // (the 3-cell row below reports 2 cells either way, just holding the wrong
    // text). These assert cell CONTENT.
    #[test]
    fn native_table_cell_keeps_a_pipe_inside_one_cell() {
        let out = md("| A | B |\n|---|---|\n| x \\| y | 2 |", true);
        let blocks = parse(&out);
        let [Block::Table { headers, rows, .. }] = blocks.as_slice() else {
            panic!("rendered {out:?}, re-parses as {blocks:?}")
        };
        assert_eq!(headers.len(), 2);
        assert_eq!(rows[0].len(), 2, "row grew extra cells: {out}");
        assert_eq!(inline_plain(&rows[0][0]), "x | y", "{out}");
        assert_eq!(inline_plain(&rows[0][1]), "2", "{out}");
    }

    #[test]
    fn native_table_cell_keeps_a_pipe_inside_a_code_span() {
        let out = md("| A | B |\n|---|---|\n| `a\\|b` | 2 |", true);
        let blocks = parse(&out);
        let [Block::Table { rows, .. }] = blocks.as_slice() else {
            panic!("rendered {out:?}, re-parses as {blocks:?}")
        };
        assert_eq!(rows[0].len(), 2, "row grew extra cells: {out}");
        // GFM strips `\|` before inline parsing, so the pipe lands INSIDE the
        // code span — still one `Code` inline, not two `Text` fragments.
        assert_eq!(rows[0][0], vec![Inline::Code("a|b".into())], "{out}");
        assert_eq!(inline_plain(&rows[0][1]), "2", "{out}");
    }

    // `\~~~~a` reaches this renderer as the bare `Text("~~~~a")` — pulldown
    // strips the escape and, because a 4-tilde run is no strikethrough delimiter,
    // hands the run over WHOLE rather than one node per tilde. Four tildes at a
    // block start are a fence: re-emitted bare they do not merely strike the
    // text, they swallow the paragraph into an empty `CodeBlock { lang:
    // Some("a") }` and the content is gone. This is what the `run >= 2` half of
    // the `~` rule guards.
    //
    // Both spellings are covered, and they arrive by different routes: `\~~~~a`
    // is handed over whole (a 4-tilde run is no strikethrough delimiter, so
    // pulldown does not split it), while the fully-escaped `\~\~\~\~a` arrives as
    // four run=1 nodes and is only seen as a run because `push_inline` coalesces
    // adjacent `Text`. The second spelling was live until that coalescing landed.
    #[test]
    fn escaped_tilde_run_does_not_reopen_a_code_fence() {
        // `round_trip_paragraph` panics if either re-parses as a CodeBlock.
        for src in [r"\~~~~a", r"\~\~\~\~a"] {
            let inlines = round_trip_paragraph(src);
            assert_eq!(inline_plain(&inlines), "~~~~a", "{src}");
        }
    }

    // The fully-escaped spelling of a strikethrough. Unlike `\~~~~a` above, a
    // FLANKING `~~` is a delimiter candidate, so pulldown hands it over
    // one-tilde-per-node (`\~\~a\~\~` -> `[Text("~"), Text("~a"), Text("~"),
    // Text("~")]`). `run`/`after` are computed per node, so the `~` rule's
    // `run >= 2` half could never see a run that spans nodes, and the tildes went
    // out bare — the platform struck text the user had explicitly escaped.
    // `push_inline` now coalesces adjacent `Text`, so the run arrives whole.
    #[test]
    fn fully_escaped_strikethrough_survives_a_round_trip_as_text() {
        let inlines = round_trip_paragraph(r"\~\~a\~\~");
        assert!(
            !inlines
                .iter()
                .any(|i| matches!(i, Inline::Strikethrough(_))),
            "text became strikethrough: {inlines:?}"
        );
        assert_eq!(inline_plain(&inlines), "~~a~~");
    }

    // The counterweight to the test above, and the reason its fix had to be
    // cross-node run tracking rather than dropping the `~` rule's `run >= 2`:
    // a LONE tilde must still go out bare. Dropping the run requirement would
    // pass the test above and put a backslash in front of every `~/path` and
    // `(~2s)` — see `ordinary_prose_is_not_over_escaped`.
    #[test]
    fn escaped_lone_tildes_are_not_escaped() {
        assert_eq!(md(r"\~a\~", false), "~a~");
    }

    // The counterweight to every test above: escaping must be *surgical*. Each
    // metacharacter here is inert exactly where it stands — `*` is surrounded by
    // spaces so it can neither open nor close emphasis, `_` is intraword (which
    // CommonMark forbids from emphasising), `-` is mid-line so it starts no
    // list, and `~` is a lone tilde where only `~~` strikes. Escaping any of
    // them would put backslashes in front of a reader for no reason.
    #[test]
    fn ordinary_prose_is_not_over_escaped() {
        let src = "Cost: $5 * 3 = $15 for the low-latency run_time_ms budget (~2s).";
        assert_eq!(md(src, false), src);
        // ...and the bare text really is inert: it survives a round trip.
        assert_eq!(inline_plain(&round_trip_paragraph(src)), src);
    }

    // `escape_anywhere`'s `'\\'` arm was the only load-bearing rule in this
    // module with no coverage: replacing it with `'\\' => false` left every
    // other test passing while silently eating the user's backslash.
    #[test]
    fn a_literal_backslash_before_punctuation_is_doubled() {
        // Source `\\\#` is the TEXT `\#` — an escaped backslash then an escaped
        // hash — so the renderer must double the backslash to carry it out.
        let src = r"\\\#";
        assert_eq!(md(src, false), r"\\#");
        // Why the doubling matters: emit a bare `\#` and it re-parses as `#`,
        // losing the backslash the user wrote.
        assert_eq!(inline_plain(&round_trip_paragraph(src)), r"\#");
    }
}
