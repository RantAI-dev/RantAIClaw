//! Reply sanitisation: what must come out of a reply before a person reads it.
//!
//! Two kinds of text the reader should never see: tool-call JSON the model emitted
//! as prose, and the runtime's own history bookkeeping, which a model can parrot
//! back out of its context (plan 355).
//!
//! The tool-call half moved out of `mod.rs` verbatim in plan 121, row 2, and is
//! still that same text.

use crate::tools::Tool;
use std::collections::HashSet;

pub(crate) fn sanitize_channel_response(response: &str, tools: &[Box<dyn Tool>]) -> String {
    let known_tool_names: HashSet<String> = tools
        .iter()
        .map(|tool| tool.name().to_ascii_lowercase())
        .collect();
    let without_tool_json = strip_isolated_tool_json_artifacts(response, &known_tool_names);
    let (without_forged_summaries, forged) = strip_forged_tool_summaries(&without_tool_json);
    if forged > 0 {
        // The count, never the text. Every one of these is a forgery by
        // definition, so how often the model does it is worth seeing.
        tracing::warn!(
            forged,
            "stripped a tool summary the model wrote; the runtime never puts its own in a delivered reply"
        );
    }
    strip_internal_history_notes(&without_forged_summaries)
}

/// Remove every `[Used tools: …]` label the model typed, and count them.
///
/// `[Used tools: …]` is the runtime's own vocabulary. It is built from the tools
/// that actually ran and added to the **history** entry, never to the delivered
/// reply, so a label inside a reply is by definition one the model wrote itself.
///
/// F-24, 2026-09-12: two Telegram replies carried one with no tool call in the
/// turn at all, at 06:38:11 and 12:20:14. The second sat above an invented
/// config file, which is what made a fabrication read like a tool's output. The
/// runtime cannot make the model honest; it can stop repeating the claim.
///
/// Wherever the label sits, not just at the start. The journal no longer keeps
/// message text (plan 352), so the exact shape that slipped past the old
/// leading-only check is not recoverable, and a fix that depends on where the
/// label sits would be guessing.
fn strip_forged_tool_summaries(message: &str) -> (String, usize) {
    const OPEN: &str = "[Used tools:";

    let mut cleaned = String::with_capacity(message.len());
    let mut rest = message;
    let mut forged = 0;

    while let Some(start) = rest.find(OPEN) {
        cleaned.push_str(&rest[..start]);
        let after_open = &rest[start + OPEN.len()..];
        // Bounded by the line, like the attachment markers: a `]` further down
        // the reply closes a different thought, and taking it would swallow
        // every line in between.
        let line_end = after_open.find('\n').unwrap_or(after_open.len());
        rest = match after_open[..line_end].find(']') {
            Some(close) => &after_open[close + 1..],
            None => &after_open[line_end..],
        };
        forged += 1;
    }
    cleaned.push_str(rest);

    // Only the blank run a stripped label can leave behind, the way
    // `strip_isolated_tool_json_artifacts` already collapses its own. Spacing
    // inside a line is left alone on purpose: the model's prose is not ours to
    // reflow, and a global squeeze would edit replies that never carried a
    // label at all.
    let mut cleaned = cleaned;
    while cleaned.contains("\n\n\n") {
        cleaned = cleaned.replace("\n\n\n", "\n\n");
    }
    (cleaned.trim().to_string(), forged)
}

/// Remove the runtime's own history bookkeeping from an outgoing reply.
///
/// `UNDELIVERED_TURN_MARKER` and its siblings are appended to history so the
/// model knows its last turn did not land. On 2026-09-12 a model read one back as
/// its answer: WhatsApp received exactly `(the previous reply was not delivered)`,
/// 38 characters, as the bot's reply to a request. Bookkeeping the model may
/// repeat must never reach a person. Every note in this family, not just that
/// one: a model that parrots one parrots the others, so the list below and the
/// family in `mod.rs` are meant to be read together.
fn strip_internal_history_notes(message: &str) -> String {
    let mut cleaned = message.to_string();
    for note in [
        super::UNDELIVERED_TURN_MARKER,
        super::UNDELIVERED_ATTACHMENT_NOTE,
        super::INTERRUPTED_TURN_MARKER,
        super::TIMED_OUT_TURN_MARKER,
        super::FAILED_TURN_MARKER,
    ] {
        cleaned = cleaned.replace(note, "");
    }
    cleaned.trim().to_string()
}

pub(crate) fn is_tool_call_payload(
    value: &serde_json::Value,
    known_tool_names: &HashSet<String>,
) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };

    let (name, has_args) =
        if let Some(function) = object.get("function").and_then(|f| f.as_object()) {
            (
                function
                    .get("name")
                    .and_then(|v| v.as_str())
                    .or_else(|| object.get("name").and_then(|v| v.as_str())),
                function.contains_key("arguments")
                    || function.contains_key("parameters")
                    || object.contains_key("arguments")
                    || object.contains_key("parameters"),
            )
        } else {
            (
                object.get("name").and_then(|v| v.as_str()),
                object.contains_key("arguments") || object.contains_key("parameters"),
            )
        };

    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return false;
    };

    has_args && known_tool_names.contains(&name.to_ascii_lowercase())
}

fn is_tool_result_payload(
    object: &serde_json::Map<String, serde_json::Value>,
    saw_tool_call_payload: bool,
) -> bool {
    if !saw_tool_call_payload || !object.contains_key("result") {
        return false;
    }

    object.keys().all(|key| {
        matches!(
            key.as_str(),
            "result" | "id" | "tool_call_id" | "name" | "tool"
        )
    })
}

fn sanitize_tool_json_value(
    value: &serde_json::Value,
    known_tool_names: &HashSet<String>,
    saw_tool_call_payload: bool,
) -> Option<(String, bool)> {
    if is_tool_call_payload(value, known_tool_names) {
        return Some((String::new(), true));
    }

    if let Some(array) = value.as_array() {
        if !array.is_empty()
            && array
                .iter()
                .all(|item| is_tool_call_payload(item, known_tool_names))
        {
            return Some((String::new(), true));
        }
        return None;
    }

    let object = value.as_object()?;

    if let Some(tool_calls) = object.get("tool_calls").and_then(|value| value.as_array()) {
        if !tool_calls.is_empty()
            && tool_calls
                .iter()
                .all(|call| is_tool_call_payload(call, known_tool_names))
        {
            let content = object
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            return Some((content, true));
        }
    }

    if is_tool_result_payload(object, saw_tool_call_payload) {
        return Some((String::new(), false));
    }

    None
}

/// Whether anything but whitespace precedes `start` on its line.
///
/// The cheap half of [`is_line_isolated_json_segment`], split out because it
/// needs only `start` — so it can reject a candidate *before* paying for a
/// parse. A `{` in the middle of prose is the common case, and the loop used to
/// parse the entire remaining message for each one, then advance a single char
/// and do it again: O(braces x bytes) on every delivered reply, for a purely
/// cosmetic strip.
pub(crate) fn json_candidate_starts_its_line(message: &str, start: usize) -> bool {
    let line_start = message[..start].rfind('\n').map_or(0, |idx| idx + 1);
    message[line_start..start].trim().is_empty()
}

fn is_line_isolated_json_segment(message: &str, start: usize, end: usize) -> bool {
    let line_end = message[end..]
        .find('\n')
        .map_or(message.len(), |idx| end + idx);

    json_candidate_starts_its_line(message, start) && message[end..line_end].trim().is_empty()
}

fn strip_isolated_tool_json_artifacts(message: &str, known_tool_names: &HashSet<String>) -> String {
    let mut cleaned = String::with_capacity(message.len());
    let mut cursor = 0usize;
    let mut saw_tool_call_payload = false;

    while cursor < message.len() {
        let Some(rel_start) = message[cursor..].find(['{', '[']) else {
            cleaned.push_str(&message[cursor..]);
            break;
        };

        let start = cursor + rel_start;
        cleaned.push_str(&message[cursor..start]);

        // Reject before parsing when the candidate cannot be line-isolated
        // anyway. This is the whole performance fix: the parse below reads the
        // entire remaining message, and without this guard every `{` in prose
        // paid for one.
        let mut stream = if json_candidate_starts_its_line(message, start) {
            Some(
                serde_json::Deserializer::from_str(&message[start..])
                    .into_iter::<serde_json::Value>(),
            )
        } else {
            None
        };

        if let Some(Ok(value)) = stream.as_mut().and_then(|s| s.next()) {
            let stream = stream.as_ref().expect("checked above");
            let consumed = stream.byte_offset();
            if consumed > 0 {
                let end = start + consumed;
                if is_line_isolated_json_segment(message, start, end) {
                    if let Some((replacement, marks_tool_call)) =
                        sanitize_tool_json_value(&value, known_tool_names, saw_tool_call_payload)
                    {
                        if marks_tool_call {
                            saw_tool_call_payload = true;
                        }
                        if !replacement.trim().is_empty() {
                            cleaned.push_str(replacement.trim());
                        }
                        cursor = end;
                        continue;
                    }
                }
            }
        }

        let Some(ch) = message[start..].chars().next() else {
            break;
        };
        cleaned.push(ch);
        cursor = start + ch.len_utf8();
    }

    let mut result = cleaned.replace("\r\n", "\n");
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_isolated_tool_json_artifacts_removes_tool_calls_and_results() {
        let mut known_tools = HashSet::new();
        known_tools.insert("schedule".to_string());

        let input = r#"{"name":"schedule","parameters":{"action":"create","message":"test"}}
{"name":"schedule","parameters":{"action":"cancel","task_id":"test"}}
Let me create the reminder properly:
{"name":"schedule","parameters":{"action":"create","message":"Go to sleep"}}
{"result":{"task_id":"abc","status":"scheduled"}}
Done reminder set for 1:38 AM."#;

        let result = strip_isolated_tool_json_artifacts(input, &known_tools);
        let normalized = result
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            normalized,
            "Let me create the reminder properly:\nDone reminder set for 1:38 AM."
        );
    }

    #[test]
    fn strip_isolated_tool_json_artifacts_preserves_non_tool_json() {
        let mut known_tools = HashSet::new();
        known_tools.insert("shell".to_string());

        let input = r#"{"name":"profile","parameters":{"timezone":"UTC"}}
This is an example JSON object for profile settings."#;

        let result = strip_isolated_tool_json_artifacts(input, &known_tools);
        assert_eq!(result, input);
    }
}
