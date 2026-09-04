//! `/calls` slash command — show what the agent did during the most
//! recently finished turn.
//!
//! Lives mostly to answer the question the user faces after a soft-cap
//! hit ("Agent exceeded maximum tool iterations"): *what were those 10
//! calls?* The inline scrollback log gives them as they happen, but
//! after the turn ends the messages scroll past; `/calls` is a single
//! place to see all of them with full args + a result preview without
//! having to scroll.

use anyhow::Result;

use super::{CommandHandler, CommandResult, OverlayContent, OverlayTab};
use crate::tui::context::TuiContext;

const MAX_ARG_VALUE_LEN: usize = 200;
const MAX_RESULT_PREVIEW_LINES: usize = 8;

pub struct CallsCommand;

impl CommandHandler for CallsCommand {
    fn name(&self) -> &str {
        "calls"
    }

    fn description(&self) -> &str {
        "Show every tool call from the most recent turn"
    }

    fn usage(&self) -> &str {
        "/calls"
    }

    fn execute(&self, _args: &str, ctx: &mut TuiContext) -> Result<CommandResult> {
        let calls = &ctx.last_turn_tool_calls;
        if calls.is_empty() {
            return Ok(CommandResult::Message(
                "No tool calls recorded yet. `/calls` shows the most recent finished turn's calls."
                    .into(),
            ));
        }

        let mut body: Vec<String> = Vec::new();
        body.push(format!(
            "Tool calls from the most recent turn ({} total):",
            calls.len()
        ));
        body.push(String::new());

        for (i, c) in calls.iter().enumerate() {
            let marker = match &c.result {
                Some((true, _)) => "▸",
                Some((false, _)) => "✗",
                None => "…",
            };
            body.push(format!("[{}] {marker} {}", i + 1, c.name));

            // Args — pretty-print scalar fields, truncate long strings.
            if let serde_json::Value::Object(map) = &c.args {
                if map.is_empty() {
                    body.push("      args: (none)".into());
                } else {
                    for (k, v) in map {
                        body.push(format!("      {k}: {}", truncate_value(v)));
                    }
                }
            } else if !c.args.is_null() {
                body.push(format!("      args: {}", truncate_value(&c.args)));
            }

            // Result preview.
            match &c.result {
                Some((ok, preview)) => {
                    let label = if *ok { "result" } else { "error" };
                    if preview.trim().is_empty() {
                        body.push(format!("      {label}: (empty)"));
                    } else {
                        for line in preview
                            .lines()
                            .take(MAX_RESULT_PREVIEW_LINES)
                            .map(|l| l.to_string())
                        {
                            body.push(format!("      {label}: {line}"));
                        }
                        let total_lines = preview.lines().count();
                        if total_lines > MAX_RESULT_PREVIEW_LINES {
                            body.push(format!(
                                "      … ({} more lines)",
                                total_lines - MAX_RESULT_PREVIEW_LINES
                            ));
                        }
                    }
                }
                None => body.push("      result: (still running or not received)".into()),
            }
            body.push(String::new());
        }

        Ok(CommandResult::Overlay(OverlayContent {
            title: format!("Calls — last turn ({} calls)", calls.len()),
            tabs: vec![OverlayTab {
                label: "calls".into(),
                body,
            }],
            active_tab: 0,
        }))
    }
}

fn truncate_value(v: &serde_json::Value) -> String {
    let s = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    // Crop by chars, not bytes: argument values are model-authored, so the cut
    // lands wherever the model put its text and a byte slice panics the overlay
    // mid-codepoint. `s.len()` was also reporting bytes under a "chars" label.
    let chars = s.chars().count();
    if chars > MAX_ARG_VALUE_LEN {
        format!(
            "{} ({chars} chars)",
            crate::tui::render::truncate_preview(&s, MAX_ARG_VALUE_LEN)
        )
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Argument values are model-authored, so the crop lands wherever the model
    /// put its text. Slicing by byte panicked the overlay on any multibyte
    /// character straddling the cut.
    #[test]
    fn truncate_value_crops_every_multibyte_width_safely() {
        for filler in ['é', '世', '🦀'] {
            for pad in (MAX_ARG_VALUE_LEN.saturating_sub(10))..(MAX_ARG_VALUE_LEN + 10) {
                let raw = format!("{}{}", "a".repeat(pad), filler.to_string().repeat(20));
                // Returning at all is the assertion: a byte crop panics.
                let out = truncate_value(&serde_json::Value::String(raw));
                assert!(!out.is_empty(), "pad {pad} filler {filler}");
            }
        }
    }

    #[test]
    fn truncate_value_keeps_short_values_whole_and_counts_characters() {
        assert_eq!(
            truncate_value(&serde_json::Value::String("ls -la".into())),
            "ls -la"
        );
        let long = "世".repeat(MAX_ARG_VALUE_LEN + 5);
        let out = truncate_value(&serde_json::Value::String(long));
        assert!(out.contains('…'), "got {out:?}");
        // The label says "chars", so the count must be characters, not bytes.
        assert!(
            out.contains(&format!("({} chars)", MAX_ARG_VALUE_LEN + 5)),
            "got {out:?}"
        );
    }

    use crate::tui::render::PersistedToolCall;

    fn ctx() -> TuiContext {
        let (ctx, _req_rx, _events_tx) = TuiContext::test_context();
        ctx
    }

    #[test]
    fn empty_calls_returns_friendly_message() {
        let mut c = ctx();
        let result = CallsCommand.execute("", &mut c).unwrap();
        match result {
            CommandResult::Message(m) => assert!(m.contains("No tool calls")),
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn calls_renders_overlay_with_each_call() {
        let mut c = ctx();
        c.last_turn_tool_calls = vec![
            PersistedToolCall {
                id: "1".into(),
                name: "shell".into(),
                args: serde_json::json!({"command": "which gog"}),
                result: Some((false, "gog: command not found".into())),
            },
            PersistedToolCall {
                id: "2".into(),
                name: "file_read".into(),
                args: serde_json::json!({"path": "/tmp/x.txt"}),
                result: Some((true, "hello world".into())),
            },
        ];
        let result = CallsCommand.execute("", &mut c).unwrap();
        match result {
            CommandResult::Overlay(o) => {
                let body = o.tabs[0].body.join("\n");
                assert!(body.contains("2 total"));
                assert!(body.contains("shell"));
                assert!(body.contains("which gog"));
                assert!(body.contains("✗"));
                assert!(body.contains("file_read"));
                assert!(body.contains("hello world"));
                assert!(body.contains("▸"));
            }
            _ => panic!("expected Overlay"),
        }
    }

    #[test]
    fn truncate_value_caps_long_strings() {
        let s = "x".repeat(500);
        let v = serde_json::Value::String(s);
        let truncated = truncate_value(&v);
        assert!(truncated.len() < 500);
        assert!(truncated.contains("…"));
        assert!(truncated.contains("500 chars"));
    }

    #[test]
    fn still_running_call_marked_with_ellipsis() {
        let mut c = ctx();
        c.last_turn_tool_calls = vec![PersistedToolCall {
            id: "1".into(),
            name: "shell".into(),
            args: serde_json::json!({"command": "ls"}),
            result: None,
        }];
        let result = CallsCommand.execute("", &mut c).unwrap();
        match result {
            CommandResult::Overlay(o) => {
                let body = o.tabs[0].body.join("\n");
                assert!(body.contains("…"));
                assert!(body.contains("still running"));
            }
            _ => panic!("expected Overlay"),
        }
    }
}
