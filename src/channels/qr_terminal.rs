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

/// Print a framed QR for `payload` to stderr, with `header` above it and a
/// reminder line below. `payload` is the raw text the phone will decode —
/// for WhatsApp Web that's the `Event::PairingQrCode { code }` value.
///
/// Uses error-correction level M (good middle-ground for screen photography
/// glare and partial occlusion) and Unicode half-block characters so the
/// QR comes out roughly square on most terminal fonts.
pub fn render_qr_with_header(payload: &str, header: &str) {
    let code = match QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M) {
        Ok(c) => c,
        Err(e) => {
            // QrCode generation can theoretically fail if the payload exceeds
            // version-40 capacity. In practice WA pairing payloads are well
            // under that, but if we ever hit it, fall back to printing the
            // raw string rather than swallowing.
            eprintln!("(could not render QR: {e})");
            eprintln!("Raw QR payload: {payload}");
            return;
        }
    };

    let art = code
        .render::<unicode::Dense1x2>()
        .dark_color(unicode::Dense1x2::Light)
        .light_color(unicode::Dense1x2::Dark)
        .build();

    eprintln!();
    eprintln!("┌─ {header} ─");
    eprintln!();
    for line in art.lines() {
        eprintln!("  {line}");
    }
    eprintln!();
    eprintln!("└─ Scan with the app's \"link a device\" or \"add device\" flow.");
    eprintln!("   If the QR is too small, increase your terminal font size.");
    eprintln!();
}

/// Print a human-readable pair code in a framed block. Used when the device
/// supports the digit-based pairing path (e.g. WhatsApp's 8-character code).
pub fn render_pair_code(code: &str) {
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

    #[test]
    fn empty_payload_doesnt_panic() {
        // Defensive: zero-length QR is valid (encodes empty string), but
        // some upstream callers may send blanks. Make sure render() doesn't
        // panic — it'll print *something* and return.
        render_qr_with_header("", "test header");
    }

    #[test]
    fn pair_code_render_doesnt_panic() {
        render_pair_code("ABCD-EFGH");
    }
}
