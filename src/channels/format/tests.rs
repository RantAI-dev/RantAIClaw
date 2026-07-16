//! End-to-end checks: the canonical tutorial (headings + table + code) renders
//! cleanly for every target, and split never orphans a fence or exceeds a limit.

use super::{render, render_pair, render_to_string, split, split_paired, LinkStyle, RenderTarget};

const FENCE: &str = "```";

fn fixture() -> String {
    format!(
        "## Penjelasan\n\nRumus **L = pi * r^2**. Lihat [docs](https://x.io).\n\n\
         - Langkah:\n  - mkdir x\n  - python3 --version\n\n\
         | Step | Perintah |\n|---|---|\n| 1 | python3 --version |\n| 2 | mkdir x |\n\n\
         {FENCE}python\nprint(1)\n{FENCE}\n"
    )
}

fn joined(target: &RenderTarget) -> String {
    split(&render(&fixture(), target), 4096).join("\n---\n")
}

#[test]
fn telegram_html_has_no_raw_markdown() {
    let out = joined(&RenderTarget::TelegramHtml);
    assert!(out.contains("<b>"));
    assert!(!out.contains("## Penjelasan"));
    assert!(!out.contains("**L"));
    assert!(out.contains("<pre>"));
}

#[test]
fn discord_std_markdown_converts_table() {
    let out = joined(&RenderTarget::StdMarkdown {
        tables_native: false,
    });
    assert!(out.contains("## Penjelasan"));
    assert!(!out.contains("| Step | Perintah |"));
    // Exactly 2 fence delimiters from the table's own CodeWrap::Fence(None) wrap
    // (opening ``` and closing ```) plus 2 from the fixture's ```python code
    // block = 4. A bare `out.contains(FENCE)` is satisfied by the code block
    // alone, so it stays true even if the table's fence wrap were dropped
    // entirely (table emitted as unwrapped prose) — this exact count is not.
    assert_eq!(
        out.matches(FENCE).count(),
        4,
        "expected 2 fences from the ASCII table + 2 from the code block: {out}"
    );
}

// `render_to_string` has no caller anywhere else in the crate (every other
// test hand-rolls `split(&render(...), N).join(...)`), so a regression in it
// (wrong render target, wrong join separator) would go undetected. Pin its
// contract against `split`'s own chunking + join, an independent code path
// that shares only `materialize`/`SEP` with the `join_all` `render_to_string`
// calls internally.
#[test]
fn render_to_string_agrees_with_render_and_split_join() {
    let target = RenderTarget::StdMarkdown {
        tables_native: false,
    };
    assert_eq!(render_to_string(&fixture(), &target), joined(&target));
}

#[test]
fn plain_strips_everything() {
    let out = joined(&RenderTarget::Plain);
    assert!(out.contains("PENJELASAN"));
    assert!(!out.contains("**"));
    assert!(!out.contains("<b>"));
}

#[test]
fn light_markup_single_asterisk() {
    let out = joined(&RenderTarget::LightMarkup {
        links: LinkStyle::Raw,
    });
    assert!(out.contains("*Penjelasan*"));
    assert!(!out.contains("**"));
}

#[test]
fn split_respects_small_limit_without_orphan_fence() {
    let chunks = split(
        &render(
            &fixture(),
            &RenderTarget::StdMarkdown {
                tables_native: false,
            },
        ),
        60,
    );
    assert!(chunks.iter().all(|c| c.chars().count() <= 60));
    assert!(chunks.iter().all(|c| c.matches(FENCE).count() % 2 == 0));
}

#[test]
fn paired_render_is_one_to_one_and_pairs_align() {
    let (html, plain) = render_pair(
        &fixture(),
        &RenderTarget::TelegramHtml,
        &RenderTarget::Plain,
    );
    assert_eq!(html.len(), plain.len());
    let pairs = split_paired(&html, &plain, 4096);
    for (h, p) in &pairs {
        assert!(!h.is_empty());
        assert!(
            !p.is_empty(),
            "fallback must exist for a non-oversized chunk"
        );
    }
}

#[test]
fn every_target_survives_pathological_input() {
    let nasty = "<script>alert(1)</script>\n\n**unclosed\n\n| a |\n|---|\n\n```\nx\n";
    for target in [
        RenderTarget::TelegramHtml,
        RenderTarget::MatrixHtml,
        RenderTarget::StdMarkdown {
            tables_native: false,
        },
        RenderTarget::StdMarkdown {
            tables_native: true,
        },
        RenderTarget::LightMarkup {
            links: LinkStyle::Slack,
        },
        RenderTarget::LightMarkup {
            links: LinkStyle::Raw,
        },
        RenderTarget::Plain,
    ] {
        let chunks = split(&render(nasty, &target), 100);
        assert!(chunks.iter().all(|c| c.chars().count() <= 100));
    }
}

// `paired_render_is_one_to_one_and_pairs_align` runs at limit 4096 — far
// bigger than the whole fixture — so `split_paired`'s splitting/flush logic
// never sees real renderer output there; only synthetic `RenderedBlock`s in
// split.rs's own unit tests stress it. limit=112 forces real splitting AND
// lands on the fixture's own naturally-occurring "measure the fallback too"
// case (split.rs:167-172): the table's TelegramHtml rendering wraps to 110
// chars (`CodeWrap::HtmlPre`, +11 fixed overhead) while its Plain rendering
// wraps to 115 (`CodeWrap::Indent`, +4 chars PER LINE across 4 lines) — the
// exact shape that comment calls out, arising from the real renderers rather
// than a hand-built fixture. At 112 the table's primary alone would fit
// (110 <= 112) but its fallback would not (115 > 112), so the two cannot be
// soundly paired: the documented contract is an EMPTY fallback for that
// piece, not a truncated or mismatched one.
#[test]
fn paired_split_forces_real_splitting_and_respects_the_empty_fallback_contract() {
    let (html, plain) = render_pair(
        &fixture(),
        &RenderTarget::TelegramHtml,
        &RenderTarget::Plain,
    );
    let limit = 112;
    let pairs = split_paired(&html, &plain, limit);

    assert!(
        pairs.len() > 1,
        "limit {limit} must force at least one split on real fixture output, got {} pair(s): {pairs:?}",
        pairs.len()
    );
    assert!(
        pairs.iter().all(|(h, _)| h.chars().count() <= limit),
        "every primary piece must respect the limit: {pairs:?}"
    );

    // The table: primary present (it's the ASCII table wrapped in <pre>), but
    // NO twin, because the Plain fallback alone would not fit.
    let table_pair = pairs
        .iter()
        .find(|(h, _)| h.contains("Perintah"))
        .expect("the table's piece must be present among the pairs");
    assert!(
        table_pair.0.contains("<pre>") && table_pair.0.chars().count() <= limit,
        "table primary must still be the real, limit-respecting ASCII table: {table_pair:?}"
    );
    assert!(
        table_pair.1.is_empty(),
        "table's fallback must be EMPTY (no sound twin exists at this limit), not mismatched: {table_pair:?}"
    );

    // Not every pair is fallback-less: blocks that DO fit together on both
    // sides (here, the trailing code block) still carry a real twin, so the
    // contract is "empty ONLY when no sound twin exists", not "empty once
    // splitting starts".
    assert!(
        pairs.iter().any(|(h, p)| !h.is_empty() && !p.is_empty()),
        "expected at least one normally-paired (non-oversized) pair alongside \
         the table's fallback-less one: {pairs:?}"
    );
}

// The original fixture had no link and no nested list, so a cross-target
// regression in link rendering (Slack `<url|text>` vs Telegram `href=` vs
// Plain `text (url)`) — or in nested-list indentation, which had a real bug
// fixed in d95f89c two commits before this fixture was written — went
// entirely uncaught by this integration layer. `fixture()` now carries both;
// this pins the per-target link dialect.
#[test]
fn link_renders_per_target_dialect() {
    let telegram = joined(&RenderTarget::TelegramHtml);
    assert!(
        telegram.contains("<a href=\"https://x.io\">docs</a>"),
        "telegram: {telegram}"
    );

    let matrix = joined(&RenderTarget::MatrixHtml);
    assert!(
        matrix.contains("<a href=\"https://x.io\">docs</a>"),
        "matrix: {matrix}"
    );

    let plain = joined(&RenderTarget::Plain);
    assert!(plain.contains("docs (https://x.io)"), "plain: {plain}");

    let markdown = joined(&RenderTarget::StdMarkdown {
        tables_native: false,
    });
    assert!(
        markdown.contains("[docs](https://x.io)"),
        "markdown: {markdown}"
    );

    let slack = joined(&RenderTarget::LightMarkup {
        links: LinkStyle::Slack,
    });
    assert!(slack.contains("<https://x.io|docs>"), "slack: {slack}");

    let raw = joined(&RenderTarget::LightMarkup {
        links: LinkStyle::Raw,
    });
    assert!(raw.contains("docs (https://x.io)"), "raw: {raw}");
}

// Cross-target regression coverage for the nested-list indentation bug fixed
// in d95f89c ("keep nested blocks' wrappers and indentation"), exercised here
// through the FULL fixture pipeline (parse -> render -> split) rather than
// the minimal "- a\n  - b" fixtures the per-renderer unit tests use.
#[test]
fn nested_list_stays_indented_across_targets() {
    let markdown = joined(&RenderTarget::StdMarkdown {
        tables_native: true,
    });
    assert!(
        markdown.contains("\n  - mkdir x") && markdown.contains("\n  - python3 --version"),
        "markdown nested list not indented: {markdown}"
    );

    let raw = joined(&RenderTarget::LightMarkup {
        links: LinkStyle::Raw,
    });
    assert!(
        raw.contains("\n  • mkdir x") && raw.contains("\n  • python3 --version"),
        "light markup nested list not indented: {raw}"
    );

    let plain = joined(&RenderTarget::Plain);
    assert!(
        plain.contains("\n  • mkdir x") && plain.contains("\n  • python3 --version"),
        "plain nested list not indented: {plain}"
    );
}

// Same input, same intent, so the same glyph. The three text targets had
// diverged for no stated reason: Plain emitted an em dash (`—`) while Telegram
// and LightMarkup emitted box-drawing (`─`).
//
// Not a claim that EVERY target agrees — the two markup targets legitimately
// differ, because each has a native rule of its own, and that is asserted here
// too so this test cannot be "unified" into a false one.
#[test]
fn text_targets_agree_on_the_rule_glyph() {
    const RULE: &str = "──────────";
    for target in [
        RenderTarget::Plain,
        RenderTarget::TelegramHtml,
        RenderTarget::LightMarkup {
            links: LinkStyle::Raw,
        },
        RenderTarget::LightMarkup {
            links: LinkStyle::Slack,
        },
    ] {
        assert_eq!(render_to_string("---", &target), RULE, "{target:?}");
    }
    assert_eq!(render_to_string("---", &RenderTarget::MatrixHtml), "<hr>");
    assert_eq!(
        render_to_string(
            "---",
            &RenderTarget::StdMarkdown {
                tables_native: true
            }
        ),
        "---"
    );
}
