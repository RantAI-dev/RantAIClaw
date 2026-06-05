//! Pure helpers for driving a tmux session: key-token translation, ANSI
//! stripping, and stable-frame detection. No IO — fully unit-testable.

use anyhow::{anyhow, Result};

/// Named keys that may be sent to a tmux pane. Names mirror `tmux send-keys`.
pub const ALLOWED_KEYS: &[&str] = &[
    "Up", "Down", "Left", "Right", "Enter", "Tab", "BTab", "Escape", "Space", "BSpace", "C-c",
    "Home", "End", "PageUp", "PageDown",
];

/// One key instruction parsed from the tool's `keys` array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyToken {
    /// A named tmux key (e.g. `Enter`, `Down`, `C-c`).
    Named(String),
    /// Literal text typed verbatim (`send-keys -l`).
    Text(String),
}

/// Parse the JSON `keys` array into validated tokens. Strings are named keys
/// (validated against [`ALLOWED_KEYS`]); objects `{ "text": "..." }` are literal text.
///
/// # Errors
/// Returns an error if the array is empty, a string is not an allowed key name,
/// or an element is neither an allowed string nor a `{ "text": "..." }` object.
pub fn parse_key_tokens(keys: &[serde_json::Value]) -> Result<Vec<KeyToken>> {
    if keys.is_empty() {
        return Err(anyhow!("keys must not be empty"));
    }
    let mut out = Vec::with_capacity(keys.len());
    for k in keys {
        match k {
            serde_json::Value::String(s) => {
                if ALLOWED_KEYS.contains(&s.as_str()) {
                    out.push(KeyToken::Named(s.clone()));
                } else {
                    return Err(anyhow!(
                        "unknown key `{s}` (allowed: {})",
                        ALLOWED_KEYS.join(", ")
                    ));
                }
            }
            serde_json::Value::Object(map) => {
                let text = map
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| anyhow!("key object must have a string `text` field"))?;
                out.push(KeyToken::Text(text.to_string()));
            }
            other => return Err(anyhow!("invalid key token: {other}")),
        }
    }
    Ok(out)
}

/// Group tokens into `tmux send-keys` argument batches. Consecutive named keys
/// share one batch; each literal-text token becomes its own `-l <text>` batch
/// so text is never interpreted as a key name.
#[must_use]
pub fn tmux_send_batches(tokens: &[KeyToken]) -> Vec<Vec<String>> {
    let mut batches: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for t in tokens {
        match t {
            KeyToken::Named(k) => current.push(k.clone()),
            KeyToken::Text(s) => {
                if !current.is_empty() {
                    batches.push(std::mem::take(&mut current));
                }
                batches.push(vec!["-l".to_string(), s.clone()]);
            }
        }
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// Strip ANSI CSI/OSC escape sequences from captured terminal text. UTF-8 safe:
/// operates on `char`s so box-drawing glyphs in TUIs survive.
#[must_use]
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: ESC [ ... final byte in @..~
            Some('[') => {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    chars.next();
                    if ('\u{40}'..='\u{7e}').contains(&nc) {
                        break;
                    }
                }
            }
            // OSC: ESC ] ... BEL or ST (ESC \)
            Some(']') => {
                chars.next();
                while let Some(nc) = chars.next() {
                    if nc == '\u{07}' {
                        break;
                    }
                    if nc == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // lone ESC or two-char escape: drop ESC and the next char
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// Normalize a captured frame for comparison: strip ANSI, trim trailing
/// whitespace per line, and drop trailing blank lines.
fn normalize_frame(s: &str) -> String {
    let stripped = strip_ansi(s);
    let mut lines: Vec<&str> = stripped.lines().map(str::trim_end).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// True if two captured frames are equal after normalization — used to detect
/// that a TUI screen has settled before sending keys.
#[must_use]
pub fn frames_stable(a: &str, b: &str) -> bool {
    normalize_frame(a) == normalize_frame(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_named_keys() {
        let toks = parse_key_tokens(&[json!("Down"), json!("Enter")]).unwrap();
        assert_eq!(
            toks,
            vec![
                KeyToken::Named("Down".into()),
                KeyToken::Named("Enter".into())
            ]
        );
    }

    #[test]
    fn parse_text_token() {
        let toks = parse_key_tokens(&[json!({"text": "hello"})]).unwrap();
        assert_eq!(toks, vec![KeyToken::Text("hello".into())]);
    }

    #[test]
    fn reject_unknown_key() {
        let err = parse_key_tokens(&[json!("Frobnicate")]).unwrap_err();
        assert!(err.to_string().contains("unknown key"));
    }

    #[test]
    fn reject_empty_keys() {
        assert!(parse_key_tokens(&[]).is_err());
    }

    #[test]
    fn batches_group_named_and_split_text() {
        let toks = vec![
            KeyToken::Named("Down".into()),
            KeyToken::Named("Down".into()),
            KeyToken::Text("prod".into()),
            KeyToken::Named("Enter".into()),
        ];
        assert_eq!(
            tmux_send_batches(&toks),
            vec![
                vec!["Down".to_string(), "Down".to_string()],
                vec!["-l".to_string(), "prod".to_string()],
                vec!["Enter".to_string()],
            ]
        );
    }

    #[test]
    fn batches_pure_named() {
        let toks = vec![
            KeyToken::Named("Down".into()),
            KeyToken::Named("Enter".into()),
        ];
        assert_eq!(
            tmux_send_batches(&toks),
            vec![vec!["Down".to_string(), "Enter".to_string()]]
        );
    }

    #[test]
    fn strip_ansi_removes_csi() {
        assert_eq!(strip_ansi("\u{1b}[31mX\u{1b}[0mY"), "XY");
    }

    #[test]
    fn strip_ansi_keeps_unicode_box() {
        let s = "┌─┐\n│ │\n└─┘";
        assert_eq!(strip_ansi(s), s);
    }

    #[test]
    fn frames_stable_ignores_trailing_ws() {
        assert!(frames_stable("a  \nb\n\n", "a\nb"));
        assert!(!frames_stable("a\nb", "a\nc"));
    }
}
