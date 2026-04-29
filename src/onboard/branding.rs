//! Brand splash — figlet banner + ASCII logo + tools/skills inventory.
//!
//! Rendered at the top of `rantaiclaw agent`, `rantaiclaw setup`, and other
//! interactive entry points so every screen reads as one product. Matches
//! the rantai-agents web palette:
//!
//! * navy `#040b2e` — logo body, dark accent
//! * sky  `#5eb8ff` — logo squares, light accent
//! * blue `#3b8cff` — primary brand accent (oklch(0.55 0.2 250))
//!
//! The banner and logo are baked at build time from
//! `scripts/branding/render_logo_ascii.py` against
//! `Logo-only Border or Stroke (1).png`. Re-run the script when the source
//! art or dimensions change.
//!
//! `NO_COLOR=1` disables truecolor escapes — we fall back to a glyph-only
//! logo (`logo_plain.txt`) and uncolored frame.

use std::fmt::Write as _;

const BANNER: &str = include_str!("assets/banner.txt");
const LOGO_ANSI: &str = include_str!("assets/logo_ansi.txt");
const LOGO_PLAIN: &str = include_str!("assets/logo_plain.txt");

const FRAME_LIGHT: char = '─';
const FRAME_TL: char = '┌';
const FRAME_TR: char = '┐';
const FRAME_BL: char = '└';
const FRAME_BR: char = '┘';
const FRAME_VR: char = '│';
const FRAME_T_DN: char = '┬';
const FRAME_T_UP: char = '┴';

/// 24-bit truecolor escape; `NO_COLOR` short-circuits.
fn fg(r: u8, g: u8, b: u8) -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        String::new()
    } else {
        format!("\x1b[38;2;{r};{g};{b}m")
    }
}

fn reset() -> &'static str {
    if std::env::var_os("NO_COLOR").is_some() {
        ""
    } else {
        "\x1b[0m"
    }
}

fn navy() -> String {
    fg(4, 11, 46)
}

fn sky() -> String {
    fg(94, 184, 255)
}

fn blue() -> String {
    fg(59, 140, 255)
}

fn dim() -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        String::new()
    } else {
        "\x1b[2m".to_string()
    }
}

fn bold() -> String {
    if std::env::var_os("NO_COLOR").is_some() {
        String::new()
    } else {
        "\x1b[1m".to_string()
    }
}

/// One logical pane in the splash.
#[derive(Debug, Clone)]
pub struct Inventory<'a> {
    /// Headline shown in the right pane: `"RantaiClaw v0.5.1"`.
    pub title: &'a str,
    /// Sub-headline: `"upstream 87615c2"` or `"profile: default"`.
    pub subtitle: Option<&'a str>,
    /// Categorized tool listings: `[("file", &["read", "write", ...]), ...]`.
    pub tools: &'a [(&'a str, &'a [&'a str])],
    /// Categorized skill listings.
    pub skills: &'a [(&'a str, &'a [&'a str])],
    /// Bottom-of-pane summary line: `"31 tools · 73 skills · /help"`.
    pub footer: Option<&'a str>,
}

impl<'a> Inventory<'a> {
    pub fn minimal(title: &'a str, subtitle: Option<&'a str>) -> Self {
        Self {
            title,
            subtitle,
            tools: &[],
            skills: &[],
            footer: None,
        }
    }
}

/// Render the figlet banner in brand sky-blue. Returns the multi-line string
/// with a trailing newline.
pub fn render_banner() -> String {
    let mut out = String::new();
    let style = format!("{}{}", bold(), sky());
    for line in BANNER.lines() {
        let _ = writeln!(out, "{style}{line}{}", reset());
    }
    out
}

/// Render the brand logo as a 24×12 character pane. Picks the colored
/// variant unless `NO_COLOR` is set.
fn logo_lines() -> Vec<String> {
    let raw = if std::env::var_os("NO_COLOR").is_some() {
        LOGO_PLAIN
    } else {
        LOGO_ANSI
    };
    raw.lines().map(str::to_string).collect()
}

/// Visible width of a string after stripping ANSI CSI sequences. We pad based
/// on visible columns so colored cells line up against the splash frame.
fn visible_width(s: &str) -> usize {
    let mut w = 0;
    let mut in_esc = false;
    for ch in s.chars() {
        if in_esc {
            if ch.is_ascii_alphabetic() {
                in_esc = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_esc = true;
            continue;
        }
        // All visible chars in our assets are 1 column wide (block drawing
        // and ASCII letters). Treat each as 1 — close enough for splash use.
        w += 1;
    }
    w
}

fn pad_visible(s: &str, target: usize) -> String {
    let w = visible_width(s);
    if w >= target {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(target - w))
    }
}

/// Render the inventory pane (right side). One line per item, never wrapped —
/// callers truncate the value list with `…` to keep things tight.
fn render_inventory_lines(inv: &Inventory<'_>) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let bold_blue = format!("{}{}", bold(), blue());
    let sky_s = sky();
    let dim_s = dim();
    let r = reset();

    let mut header = format!("{bold_blue}{}{r}", inv.title);
    if let Some(sub) = inv.subtitle {
        header.push_str(&format!(" {dim_s}· {sub}{r}"));
    }
    lines.push(header);

    if !inv.tools.is_empty() {
        lines.push(String::new());
        lines.push(format!("{bold_blue}Available Tools{r}"));
        for (cat, items) in inv.tools {
            let joined = items.join(", ");
            lines.push(format!("{sky_s}{cat}:{r} {joined}"));
        }
    }

    if !inv.skills.is_empty() {
        lines.push(String::new());
        lines.push(format!("{bold_blue}Available Skills{r}"));
        for (cat, items) in inv.skills {
            let joined = items.join(", ");
            lines.push(format!("{sky_s}{cat}:{r} {joined}"));
        }
    }

    if let Some(footer) = inv.footer {
        lines.push(String::new());
        lines.push(format!("{dim_s}{footer}{r}"));
    }

    lines
}

/// Compose banner + framed two-pane splash (logo left, inventory right).
///
/// Width budget: banner is whatever pyfiglet produced (~72 cols); frame
/// targets 92 cols by default — wide enough for the tools/skills summary
/// but still comfortable on an 100-col terminal.
pub fn render_splash(inv: &Inventory<'_>) -> String {
    const FRAME_WIDTH: usize = 92;
    const LOGO_PANE_W: usize = 26;
    const RIGHT_PANE_W: usize = FRAME_WIDTH - LOGO_PANE_W - 3; // 2 vert frames + 1 separator

    let banner = render_banner();
    let logo = logo_lines();
    let inv_lines = render_inventory_lines(inv);
    let pane_height = logo.len().max(inv_lines.len());

    let frame_color = navy();
    let r = reset();

    let top = format!(
        "{frame_color}{FRAME_TL}{}{FRAME_T_DN}{}{FRAME_TR}{r}",
        FRAME_LIGHT.to_string().repeat(LOGO_PANE_W),
        FRAME_LIGHT.to_string().repeat(RIGHT_PANE_W),
    );
    let bottom = format!(
        "{frame_color}{FRAME_BL}{}{FRAME_T_UP}{}{FRAME_BR}{r}",
        FRAME_LIGHT.to_string().repeat(LOGO_PANE_W),
        FRAME_LIGHT.to_string().repeat(RIGHT_PANE_W),
    );

    let mut out = String::new();
    out.push('\n');
    out.push_str(&banner);
    out.push_str(&top);
    out.push('\n');
    for i in 0..pane_height {
        let left = logo.get(i).cloned().unwrap_or_default();
        let right = inv_lines.get(i).cloned().unwrap_or_default();
        let left_padded = pad_visible(&left, LOGO_PANE_W);
        let right_padded = pad_visible(&right, RIGHT_PANE_W);
        let _ = writeln!(
            out,
            "{frame_color}{FRAME_VR}{r}{left_padded}{frame_color}{FRAME_VR}{r}{right_padded}{frame_color}{FRAME_VR}{r}",
        );
    }
    out.push_str(&bottom);
    out.push('\n');
    out
}

/// Convenience: a no-bells splash for short-lived commands (`--version`,
/// help, etc.) — banner only, no frame.
pub fn render_banner_only() -> String {
    let mut out = String::new();
    out.push('\n');
    out.push_str(&render_banner());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_includes_all_letters() {
        let b = render_banner();
        // ANSI Shadow uses block chars; visible-width-stripping check the
        // letters spell RANTAICLAW (6 lines tall, all letters present).
        assert!(b.contains("██████"), "banner should contain block chars");
        assert!(b.lines().count() >= 6);
    }

    #[test]
    fn splash_frames_align() {
        let inv = Inventory::minimal("RantaiClaw v0.5.1", Some("test"));
        let s = render_splash(&inv);
        // Top + bottom frame lines must contain the same width characters.
        let lines: Vec<&str> = s.lines().collect();
        let top = lines.iter().find(|l| l.contains('┐')).unwrap();
        let bot = lines.iter().find(|l| l.contains('┘')).unwrap();
        assert_eq!(visible_width(top), visible_width(bot));
    }

    #[test]
    fn inventory_renders_tool_categories() {
        let tools: &[(&str, &[&str])] = &[
            ("file", &["read", "write"]),
            ("net", &["fetch"]),
        ];
        let inv = Inventory {
            title: "RC",
            subtitle: None,
            tools,
            skills: &[],
            footer: None,
        };
        let lines = render_inventory_lines(&inv);
        assert!(lines.iter().any(|l| l.contains("file:")));
        assert!(lines.iter().any(|l| l.contains("read, write")));
        assert!(lines.iter().any(|l| l.contains("net:")));
    }

    #[test]
    fn no_color_disables_escapes() {
        let prev = std::env::var_os("NO_COLOR");
        std::env::set_var("NO_COLOR", "1");
        let inv = Inventory::minimal("RC", None);
        let s = render_splash(&inv);
        // Restore env before assertion so a panic still cleans up.
        match prev {
            Some(v) => std::env::set_var("NO_COLOR", v),
            None => std::env::remove_var("NO_COLOR"),
        }
        assert!(!s.contains("\x1b["), "NO_COLOR splash must contain no ANSI: got {s:?}");
    }

    #[test]
    fn visible_width_strips_ansi() {
        let raw = "\x1b[31mhello\x1b[0m";
        assert_eq!(visible_width(raw), 5);
    }
}
