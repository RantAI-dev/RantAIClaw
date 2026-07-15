//! End-to-end checks: the canonical tutorial (headings + table + code) renders
//! cleanly for every target, and split never orphans a fence or exceeds a limit.

use super::{render, render_pair, split, split_paired, LinkStyle, RenderTarget};

const FENCE: &str = "```";

fn fixture() -> String {
    format!(
        "## Penjelasan\n\nRumus **L = pi * r^2**.\n\n\
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
    assert!(out.contains(FENCE));
    assert!(!out.contains("| Step | Perintah |"));
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
