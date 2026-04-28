//! Renderers for `Vec<CheckResult>` — text, JSON, and brief.

use serde_json::{json, Value};

use crate::doctor::{CheckResult, Severity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorFormat {
    Text,
    Json,
    Brief,
}

pub fn render(results: &[CheckResult], format: DoctorFormat) -> String {
    match format {
        DoctorFormat::Text => render_text(results, false),
        DoctorFormat::Json => render_json_string(results),
        DoctorFormat::Brief => render_brief(results),
    }
}

pub fn render_text(results: &[CheckResult], colors: bool) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(out, "RantaiClaw Doctor");
    let _ = writeln!(out, "─────────────────");

    let mut current_cat = "";
    for r in results {
        if r.category != current_cat {
            current_cat = r.category;
            let _ = writeln!(out);
            let _ = writeln!(out, "[{current_cat}]");
        }
        let icon = colorize(r.severity, r.severity.icon(), colors);
        let _ = writeln!(out, "  {icon} {} — {}", r.name, r.message);
        if let Some(hint) = &r.hint {
            let arrow = colorize_arrow(colors);
            let _ = writeln!(out, "      {arrow} {hint}");
        }
    }

    let totals = totals(results);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Summary: {} ok, {} warn, {} fail, {} info",
        totals.ok, totals.warn, totals.fail, totals.info
    );
    out
}

pub fn render_brief(results: &[CheckResult]) -> String {
    let t = totals(results);
    let total = results.len();
    format!("doctor: {}/{} ok, {} warn, {} fail", t.ok, total, t.warn, t.fail)
}

pub fn render_json(results: &[CheckResult]) -> Value {
    let items: Vec<Value> = results
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "severity": r.severity.as_str(),
                "message": r.message,
                "hint": r.hint,
                "category": r.category,
                "duration_ms": r.duration_ms,
            })
        })
        .collect();

    let t = totals(results);
    json!({
        "results": items,
        "summary": {
            "total": results.len(),
            "ok": t.ok,
            "warn": t.warn,
            "fail": t.fail,
            "info": t.info,
        }
    })
}

fn render_json_string(results: &[CheckResult]) -> String {
    serde_json::to_string_pretty(&render_json(results))
        .unwrap_or_else(|_| "{}".to_string())
}

#[derive(Debug, Default, Clone, Copy)]
struct Totals {
    ok: usize,
    warn: usize,
    fail: usize,
    info: usize,
}

fn totals(results: &[CheckResult]) -> Totals {
    let mut t = Totals::default();
    for r in results {
        match r.severity {
            Severity::Ok => t.ok += 1,
            Severity::Warn => t.warn += 1,
            Severity::Fail => t.fail += 1,
            Severity::Info => t.info += 1,
        }
    }
    t
}

fn colorize(sev: Severity, icon: &str, colors: bool) -> String {
    if !colors {
        return icon.to_string();
    }
    let code = match sev {
        Severity::Ok => "\x1b[32m",
        Severity::Warn => "\x1b[33m",
        Severity::Fail => "\x1b[31m",
        Severity::Info => "\x1b[36m",
    };
    format!("{code}{icon}\x1b[0m")
}

fn colorize_arrow(colors: bool) -> String {
    if colors { "\x1b[2m→\x1b[0m".to_string() } else { "→".to_string() }
}

pub fn has_failures(results: &[CheckResult]) -> bool {
    results.iter().any(|r| r.severity == Severity::Fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<CheckResult> {
        vec![
            CheckResult::ok("config.schema", "config schema is valid"),
            CheckResult::warn("policy.allowlist", "allowlist is empty")
                .with_hint("run: rantaiclaw setup approvals"),
            CheckResult::fail("provider.ping", "401 unauthorized")
                .with_category("live")
                .with_hint("re-enter API key with: rantaiclaw setup provider"),
        ]
    }

    #[test]
    fn brief_one_liner_format_is_stable() {
        let s = render_brief(&fixture());
        assert_eq!(s, "doctor: 1/3 ok, 1 warn, 1 fail");
    }

    #[test]
    fn text_renderer_shows_arrow_for_hints() {
        let s = render_text(&fixture(), false);
        assert!(s.contains("→ run: rantaiclaw setup approvals"));
        assert!(s.contains("✓ config.schema"));
        assert!(s.contains("⚠ policy.allowlist"));
        assert!(s.contains("✗ provider.ping"));
    }

    #[test]
    fn text_renderer_groups_by_category() {
        let s = render_text(&fixture(), false);
        let cfg_pos = s.find("[config]").expect("config header");
        let live_pos = s.find("[live]").expect("live header");
        assert!(cfg_pos < live_pos, "config must precede live");
    }

    #[test]
    fn json_renderer_emits_summary_and_results() {
        let v = render_json(&fixture());
        assert_eq!(v["summary"]["total"], 3);
        assert_eq!(v["summary"]["fail"], 1);
        assert_eq!(v["results"][0]["severity"], "ok");
        assert_eq!(v["results"][1]["hint"], "run: rantaiclaw setup approvals");
    }

    #[test]
    fn has_failures_returns_true_only_for_fail() {
        let oks = vec![CheckResult::ok("a", "b")];
        assert!(!has_failures(&oks));
        assert!(has_failures(&fixture()));
    }
}
