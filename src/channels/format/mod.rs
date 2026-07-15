//! Per-platform rendering of the agent's GitHub-Flavored-Markdown replies.
//!
//! `render` parses GFM once (see [`ast`]) and turns it into a list of
//! [`RenderedBlock`]s formatted for a specific [`RenderTarget`]. `split` packs
//! those blocks into platform-sized chunks without cutting a code fence or an
//! HTML tag. Pure and deterministic: no clock, no randomness.
//!
//! Invariant: every renderer emits exactly one [`RenderedBlock`] per input
//! block, in order. [`split_paired`] relies on it.

mod ast;
mod html;
mod light;
mod markdown;
mod nest;
mod plain;
mod split;
mod table;

#[cfg(test)]
mod tests;

/// How links are emitted by the [`RenderTarget::LightMarkup`] renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStyle {
    /// Slack `<url|text>`.
    Slack,
    /// No link markup — `text (url)`.
    Raw,
}

/// The output format a channel wants. Each channel maps to exactly one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderTarget {
    /// Telegram `parse_mode=HTML`: `<b><i><u><s><code><pre><a>`, headings→`<b>`,
    /// tables→`<pre>` ASCII, lists→`• ` lines.
    TelegramHtml,
    /// Matrix `org.matrix.custom.html`: headings→`<h1>..<h6>`, lists→`<ul>/<ol>`,
    /// tables→`<pre>` ASCII (client `<table>` support is inconsistent).
    MatrixHtml,
    /// CommonMark-ish markdown kept mostly intact.
    StdMarkdown {
        /// `true` keeps native `| a | b |` tables (Mattermost); `false` renders
        /// tables as aligned ASCII in a fenced block (Discord, DingTalk).
        tables_native: bool,
    },
    /// WhatsApp/Slack-style single-char markup: `*bold*` `_italic_` `~strike~`.
    LightMarkup { links: LinkStyle },
    /// Plain text: all markup stripped to readable text.
    Plain,
}

/// Whether a rendered block is prose, HTML prose (split only at tag depth 0), or
/// code (re-wrapped with its fence on every chunk).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Prose,
    ProseHtml,
    Code,
}

/// How a [`BlockKind::Code`] block is wrapped when emitted (or re-emitted after
/// an oversized-code split).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeWrap {
    /// ```` ```lang ```` fence.
    Fence(Option<String>),
    /// `<pre>` … `</pre>`.
    HtmlPre,
    /// Four-space indent per line.
    Indent,
}

/// One fully-rendered block. For prose, `text` is the final platform text. For
/// `Code`, `text` is the RAW code (no fence) and `code_wrap` says how to wrap it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedBlock {
    pub kind: BlockKind,
    pub text: String,
    pub code_wrap: Option<CodeWrap>,
}

impl RenderedBlock {
    pub fn prose(text: impl Into<String>) -> Self {
        Self {
            kind: BlockKind::Prose,
            text: text.into(),
            code_wrap: None,
        }
    }
    pub fn prose_html(text: impl Into<String>) -> Self {
        Self {
            kind: BlockKind::ProseHtml,
            text: text.into(),
            code_wrap: None,
        }
    }
    pub fn code(text: impl Into<String>, wrap: CodeWrap) -> Self {
        Self {
            kind: BlockKind::Code,
            text: text.into(),
            code_wrap: Some(wrap),
        }
    }
}

/// Parse `md` as GFM and render it for `target`.
pub fn render(md: &str, target: &RenderTarget) -> Vec<RenderedBlock> {
    let blocks = ast::parse(md);
    render_blocks(&blocks, target)
}

/// Render an already-parsed AST. Use this (with one `ast::parse`) when you need
/// two renderings of the SAME source — see [`split_paired`].
///
/// `pub(crate)`: `ast::Block` is private to `format`, so no caller outside this
/// module could construct an argument for it anyway.
pub(crate) fn render_blocks(blocks: &[ast::Block], target: &RenderTarget) -> Vec<RenderedBlock> {
    match target {
        RenderTarget::TelegramHtml => html::render_telegram(blocks),
        RenderTarget::MatrixHtml => html::render_matrix(blocks),
        RenderTarget::StdMarkdown { tables_native } => markdown::render(blocks, *tables_native),
        RenderTarget::LightMarkup { links } => light::render(blocks, *links),
        RenderTarget::Plain => plain::render(blocks),
    }
}

/// Parse once and render to two targets. The two lists are 1:1 with each other
/// (renderer invariant), which is what makes [`split_paired`] sound.
pub fn render_pair(
    md: &str,
    primary: &RenderTarget,
    fallback: &RenderTarget,
) -> (Vec<RenderedBlock>, Vec<RenderedBlock>) {
    let blocks = ast::parse(md);
    (
        render_blocks(&blocks, primary),
        render_blocks(&blocks, fallback),
    )
}

/// Convenience: render to a single `String` (unchunked).
pub fn render_to_string(md: &str, target: &RenderTarget) -> String {
    split::join_all(&render(md, target))
}

// `cargo clippy --all-targets` compiles this module twice: once as part of the
// lib crate (`src/lib.rs: pub mod channels;`) and once as the bin crate's own
// private copy (`src/main.rs: mod channels;`). The lib unit is exempt from
// `unused_imports` on `pub` re-exports because a lib crate might be used by a
// downstream crate; a bin crate has no downstream, so that exemption never
// applies there. Neither `split`'s own module visibility nor this re-export's
// visibility changes that — verified empirically (private/`pub(crate)`/`pub`
// all warn identically) — because the bin's `mod channels;` is itself private,
// which breaks external reachability regardless of how `pub` anything nested
// under it is. The only real consumers right now are this crate's own test
// suite (`tests.rs`) and, once PR2 lands, sibling channel modules under
// `src/channels/*.rs` — both reach this via the re-export below, not via
// `split`'s own (deliberately private) module path.
#[allow(unused_imports)]
pub use split::{split, split_paired};
