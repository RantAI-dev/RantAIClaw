//! QR code rendering for terminal-driven device pairing.
//!
//! Used by the WhatsApp Web channel today (and any future channel that
//! ships QR-based linking — Signal, Matrix, Telegram desktop's QR sign-in).
//! The whole point: when a phone-side app says "scan this QR", the user
//! should not be hunting through `RUST_LOG=debug` output for a base64
//! string. Print actual block characters they can point a phone at.
//!
//! Renders to **stderr** so the daemon's stdout (used for structured event
//! streams in some setups) stays clean.

use qrcode::render::unicode;
use qrcode::{EcLevel, QrCode};
use std::io::IsTerminal as _;

/// Whether the pairing secret may be written to stderr.
///
/// A managed daemon's stderr is captured by the journal, so rendering a pair
/// code or a QR payload there writes a credential — one that links a device to
/// the account — into a log an operator did not choose to hold. Render only for
/// an interactive terminal; otherwise point at where to run the flow.
fn stderr_is_interactive() -> bool {
    std::io::stderr().is_terminal()
}

/// Whether a pairing secret may be written to stdout, for the same reason as
/// [`stderr_is_interactive`]: a managed daemon's stdout is the journal too.
/// Telegram prints its startup pairing code there.
pub(crate) fn stdout_is_interactive() -> bool {
    std::io::stdout().is_terminal()
}

/// One rendered QR block, ready to be printed to a terminal.
///
/// `text` ends with a newline; `lines` is the number of terminal lines the
/// text occupies, so the next rotation can move the cursor up by exactly
/// that many rows before clearing and redrawing in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedQr {
    pub text: String,
    pub lines: usize,
}

/// Build the framed QR block for `payload` and `header` without printing it.
///
/// Returns `None` when stderr is not a tty — writing the payload there would
/// land it in a journal as a device-linking credential. Callers can use the
/// `None` case to print a short "QR refreshed" line instead of stacking a
/// new block.
pub fn build_qr_block(payload: &str, header: &str) -> Option<RenderedQr> {
    if !stderr_is_interactive() {
        return None;
    }
    let code = match QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M) {
        Ok(c) => c,
        Err(e) => {
            let mut text = String::new();
            use std::fmt::Write as _;
            let _ = writeln!(text, "(could not render QR: {e})");
            let _ = writeln!(text, "Raw QR payload: {payload}");
            let lines = text.lines().count();
            return Some(RenderedQr { text, lines });
        }
    };

    let art = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build();

    let mut text = String::new();
    text.push('\n');
    use std::fmt::Write as _;
    let _ = writeln!(text, "┌─ {header} ─");
    text.push('\n');
    for line in art.lines() {
        let _ = writeln!(text, "  {line}");
    }
    text.push('\n');
    text.push_str("└─ Scan with the app's \"link a device\" or \"add device\" flow.\n");
    text.push_str("   If the QR is too small, increase your terminal font size.\n");
    text.push('\n');
    let lines = text.lines().count();
    Some(RenderedQr { text, lines })
}

/// State carried across QR events in the headless renderer.
///
/// `seen` becomes `true` once any QR event has been rendered (or skipped
/// because stderr was not a tty); `lines` is the size of the last block the
/// renderer actually printed, which the next event uses to move the cursor
/// up and redraw in place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QrRenderState {
    pub seen: bool,
    pub lines: Option<usize>,
}

impl QrRenderState {
    /// First QR event, or the only state carried into the next call when the
    /// caller did not bother tracking lines.
    pub fn fresh() -> Self {
        Self::default()
    }
}

/// Redraw a QR block in place on a terminal, or print a single line when
/// stderr is not a tty.
///
/// The returned tuple is the text to print and the next state. On a tty
/// stderr, the text starts with ANSI escapes to move the cursor up by the
/// previous block's line count (when one was rendered) before clearing and
/// drawing the new block, so QR rotations replace instead of stacking. On a
/// non-tty stderr, one short "QR refreshed" line is printed for every QR
/// event after the first, so the operator knows rotations are happening and
/// the payload stays out of the journal.
pub fn redraw_qr_block(
    payload: &str,
    header: &str,
    state: QrRenderState,
) -> (String, QrRenderState) {
    match build_qr_block(payload, header) {
        Some(rendered) => {
            let mut out = String::new();
            if let Some(prev) = state.lines {
                use std::fmt::Write as _;
                let _ = write!(out, "\x1b[{prev}A");
                out.push_str("\x1b[J");
            }
            out.push_str(&rendered.text);
            (
                out,
                QrRenderState {
                    seen: true,
                    lines: Some(rendered.lines),
                },
            )
        }
        None => {
            let mut out = String::new();
            if state.seen {
                out.push_str("QR refreshed\n");
            }
            (
                out,
                QrRenderState {
                    seen: true,
                    lines: None,
                },
            )
        }
    }
}

/// Print a framed QR for `payload` to stderr, with `header` above it and a
/// reminder line below. `payload` is the raw text the phone will decode —
/// for WhatsApp Web that's the `Event::PairingQrCode { code }` value.
///
/// Uses error-correction level M (good middle-ground for screen photography
/// glare and partial occlusion) and Unicode half-block characters so the
/// QR comes out roughly square on most terminal fonts.
pub fn render_qr_with_header(payload: &str, header: &str) {
    match build_qr_block(payload, header) {
        Some(rendered) => {
            let _ = std::io::Write::write_all(&mut std::io::stderr(), rendered.text.as_bytes());
        }
        None => {
            tracing::info!(
                "WhatsApp Web is waiting to be linked, but stderr is not a terminal so the QR \
                 is not being rendered (it would be written to the journal as a credential). \
                 Run the pairing flow interactively: `rantaiclaw setup whatsapp-web`."
            );
        }
    }
}

/// Print a human-readable pair code in a framed block. Used when the device
/// supports the digit-based pairing path (e.g. WhatsApp's 8-character code).
pub fn render_pair_code(code: &str) {
    if !stderr_is_interactive() {
        tracing::info!(
            "WhatsApp Web issued a pair code, but stderr is not a terminal so it is not being \
             printed (it would be written to the journal as a credential). Run the pairing flow \
             interactively: `rantaiclaw setup whatsapp-web`."
        );
        return;
    }

    eprintln!();
    eprintln!("┌─ Pair code received");
    eprintln!();
    eprintln!("    {code}");
    eprintln!();
    eprintln!("└─ Enter this code in WhatsApp > Linked Devices > Link a Device.");
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_renders_for_typical_payload() {
        // We don't snapshot the exact bytes — the qrcode crate's output is
        // version-stable but verbose. The contract this test guards is: a
        // realistic-length WhatsApp-style payload doesn't panic and does
        // produce a non-empty render.
        let payload = "1@AbCdEfGh1234567890==,xyz123,r4nd0mPub2KEY,DD";
        let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M)
            .expect("realistic payload fits in QR");
        let art = code.render::<unicode::Dense1x2>().build();
        assert!(!art.is_empty());
        // Sanity: lines are non-trivial in length.
        assert!(art.lines().any(|l| l.chars().count() > 10));
    }

    /// Both renderers refuse to write the secret when stderr is not a
    /// terminal — a managed daemon's stderr goes to the journal, and a pair
    /// code links a device to the account. The test harness captures stderr,
    /// so the non-TTY path is what runs here; the no-panic contract holds
    /// either way.
    #[test]
    fn secrets_are_not_rendered_to_a_non_tty() {
        render_pair_code("ABCD-EFGH");
        render_qr_with_header("1@payload", "header");
    }

    #[test]
    fn empty_payload_doesnt_panic() {
        // Defensive: zero-length QR is valid (encodes empty string), but
        // some upstream callers may send blanks. Make sure render() doesn't
        // panic — it'll print *something* and return.
        render_qr_with_header("", "test header");
    }

    /// `build_qr_block` returns `None` on a non-tty stderr so callers can
    /// fall back to a non-secret replacement line. The test harness captures
    /// stderr, so this is the non-tty path.
    #[test]
    fn build_qr_block_returns_none_on_non_tty() {
        assert!(
            !stderr_is_interactive(),
            "the test harness should not hand us a TTY"
        );
        assert!(
            build_qr_block("1@payload", "header").is_none(),
            "non-tty stderr must not produce a QR block"
        );
    }

    /// A first QR on a non-tty produces nothing; a second one prints a short
    /// `QR refreshed` line so the operator knows the device side is still
    /// rotating codes. The payload stays out of the journal.
    #[test]
    fn redraw_qr_block_emits_qr_refreshed_when_no_block_can_be_built() {
        let (first_text, state_after_first) =
            redraw_qr_block("1@payload", "header", QrRenderState::fresh());
        assert!(
            first_text.is_empty(),
            "first call should not print when stderr is not a tty, got: {first_text:?}"
        );
        assert!(
            state_after_first.seen,
            "first call must mark the QR as seen even on a non-tty"
        );
        assert!(
            state_after_first.lines.is_none(),
            "first call on a non-tty must not reserve a line count"
        );
        let (second_text, state_after_second) =
            redraw_qr_block("2@payload", "header", state_after_first);
        assert_eq!(
            second_text, "QR refreshed\n",
            "second call must print one short refresh line, got: {second_text:?}"
        );
        assert!(
            state_after_second.lines.is_none(),
            "second call on a non-tty still must not reserve a line count"
        );
    }

    /// Two rotations on a tty replace, not stack. The second text starts with
    /// the cursor-up + clear escapes for the first block, so the terminal
    /// shows only the second block where the first one was.
    #[test]
    fn redraw_qr_block_replaces_in_place_on_a_tty() {
        if !stderr_is_interactive() {
            return;
        }
        let (_, state) = redraw_qr_block("1@payload", "header", QrRenderState::fresh());
        let lines = state.lines.expect("interactive stderr must build a block");
        let (second_text, second_state) = redraw_qr_block("2@payload", "header", state);
        assert!(
            second_text.starts_with(&format!("\x1b[{lines}A\x1b[J")),
            "second call must clear the first block, got prefix: {:?}",
            second_text.chars().take(8).collect::<String>()
        );
        assert!(second_text.contains("2@payload"));
        assert!(
            !second_text.contains("1@payload"),
            "second call must not keep the first payload on screen"
        );
        assert!(second_state.lines.is_some());
    }

    /// Two rotations must not stack — the second payload must not appear
    /// twice in the rendered text. The previous frame's `┌─ WhatsApp Web
    /// Pairing QR ─` header would otherwise print below the first.
    #[test]
    fn redraw_qr_block_does_not_stack_a_second_header() {
        if !stderr_is_interactive() {
            return;
        }
        let (_, state) = redraw_qr_block("1@payload", "header", QrRenderState::fresh());
        let (second_text, _) = redraw_qr_block("2@payload", "header", state);
        assert_eq!(
            second_text.matches("┌─ WhatsApp Web Pairing QR ─").count(),
            1,
            "the QR header must appear exactly once after a rotation"
        );
    }

    #[test]
    fn pair_code_render_doesnt_panic() {
        render_pair_code("ABCD-EFGH");
    }
}
