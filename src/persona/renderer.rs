//! Persona template renderer — substring substitution + `{{#if <var>}}` block guards.
//!
//! This is intentionally not a templating engine. The 5 bundled persona
//! markdown templates have a fixed, hand-curated set of placeholders
//! (`{{name}}`, `{{timezone}}`, `{{role}}`, `{{tone}}`, `{{avoid}}`) plus two
//! block guards, `{{#if avoid}}...{{/if}}` and `{{#if timezone}}...{{/if}}`.
//! A pure-string approach is easier to audit, has no run-time dependency
//! surface, and produces deterministic output that round-trips through
//! snapshot tests.
//!
//! See `docs/superpowers/specs/2026-04-27-onboarding-depth-v2-design.md`,
//! §"Section 3 — persona (NEW)".

/// Render `template` with the given substitutions.
///
/// `avoid` is `Some(non-empty)` to keep the avoid block, or `None` /
/// `Some("")` / `Some(whitespace-only)` to strip it entirely (block guards
/// removed and the inner sentence dropped).
pub fn render(
    template: &str,
    name: &str,
    timezone: &str,
    role: &str,
    tone: &str,
    avoid: Option<&str>,
) -> String {
    // First decide whether each conditional block should survive.
    let keep_avoid = avoid.map(|s| !s.trim().is_empty()).unwrap_or(false);
    let keep_timezone = !timezone.trim().is_empty();

    let stripped = if keep_timezone {
        keep_if_block(template, "timezone")
    } else {
        strip_if_block(template, "timezone")
    };
    let stripped = if keep_avoid {
        keep_if_block(&stripped, "avoid")
    } else {
        strip_if_block(&stripped, "avoid")
    };

    // A single left-to-right pass resolves each `{{key}}` exactly once, so a
    // value that itself contains a `{{placeholder}}` token is not re-expanded by
    // a later replacement (which sequential `.replace` calls would have done).
    // An empty name falls back to "you" so a legacy persona with no name does
    // not render a double-space gap ("assistant for  (timezone: …)").
    let name = if name.trim().is_empty() { "you" } else { name };
    let avoid_value = avoid.unwrap_or("");
    substitute_placeholders(&stripped, |key| match key {
        "name" => Some(name),
        "timezone" => Some(timezone),
        "role" => Some(role),
        "tone" => Some(tone),
        "avoid" => Some(avoid_value),
        _ => None,
    })
}

/// Walk `template`, replacing each `{{key}}` once with `lookup(key)`. An
/// unknown key (no match from `lookup`) is emitted verbatim, so a value that
/// happens to contain `{{…}}` is never re-expanded.
fn substitute_placeholders<'a>(template: &str, lookup: impl Fn(&str) -> Option<&'a str>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        if let Some(close) = after.find("}}") {
            let key = after[..close].trim();
            match lookup(key) {
                Some(value) => out.push_str(value),
                None => {
                    // Not a known placeholder — emit the literal `{{…}}`.
                    out.push_str(&rest[open..open + 2 + close + 2]);
                }
            }
            rest = &after[close + 2..];
        } else {
            // No closing `}}` — emit the rest verbatim.
            out.push_str(&rest[open..]);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

/// The opening marker for a `{{#if <var>}}...{{/if}}` block guard.
fn if_open_tag(var: &str) -> String {
    let mut s = String::from("{{#if ");
    s.push_str(var);
    s.push_str("}}");
    s
}

/// Remove `{{#if <var>}}...{{/if}}` (and its trailing blank line, if any) from
/// the template, dropping the block's contents along with the markers.
/// Operates on raw source text — no regex dependency. Only the block guarded
/// by `var` is touched; another variable's `{{#if ...}}...{{/if}}` block
/// elsewhere in the template is left alone.
fn strip_if_block(template: &str, var: &str) -> String {
    let open = if_open_tag(var);
    const CLOSE: &str = "{{/if}}";

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open_idx) = rest.find(open.as_str()) {
        out.push_str(&rest[..open_idx]);
        let after_open = &rest[open_idx + open.len()..];
        if let Some(close_idx) = after_open.find(CLOSE) {
            // Drop the block contents entirely.
            let after_close = &after_open[close_idx + CLOSE.len()..];
            // Consume one trailing newline immediately after `{{/if}}` so we
            // don't leave a lone blank line where the block used to live.
            let after_close = after_close.strip_prefix('\n').unwrap_or(after_close);
            // Pull a stacked blank line above the (now-removed) block back to
            // a single blank line so the template doesn't grow vertical gaps.
            if out.ends_with("\n\n") {
                out.pop();
            }
            rest = after_close;
        } else {
            // Unterminated guard — leave the opener in place; consumers will
            // see the literal markers and that's loud enough to debug.
            out.push_str(&open);
            rest = after_open;
        }
    }
    out.push_str(rest);
    out
}

/// Keep a `{{#if <var>}}...{{/if}}` block's contents, removing only that
/// variable's own opening and closing markers. Another variable's block
/// guard elsewhere in the template — its own `{{#if ...}}` and the
/// `{{/if}}` that closes it — is left untouched, so this never strips a
/// sibling block's closer.
fn keep_if_block(template: &str, var: &str) -> String {
    let open = if_open_tag(var);
    const CLOSE: &str = "{{/if}}";

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open_idx) = rest.find(open.as_str()) {
        out.push_str(&rest[..open_idx]);
        let after_open = &rest[open_idx + open.len()..];
        // Drop the opening marker itself (plus one trailing newline, if
        // any) — the content up to this variable's own closer survives.
        let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
        if let Some(close_idx) = after_open.find(CLOSE) {
            out.push_str(&after_open[..close_idx]);
            let after_close = &after_open[close_idx + CLOSE.len()..];
            let after_close = after_close.strip_prefix('\n').unwrap_or(after_close);
            rest = after_close;
        } else {
            // Unterminated guard — leave the opener in place.
            out.push_str(&open);
            rest = after_open;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "intro\n\n{{#if avoid}}\nThings to avoid: {{avoid}}\n{{/if}}\n\noutro\n";

    #[test]
    fn avoid_none_strips_block() {
        let out = render(SAMPLE, "n", "tz", "r", "neutral", None);
        assert!(!out.contains("Things to avoid"));
        assert!(!out.contains("{{#if"));
        assert!(!out.contains("{{/if"));
    }

    #[test]
    fn avoid_empty_strips_block() {
        let out = render(SAMPLE, "n", "tz", "r", "neutral", Some(""));
        assert!(!out.contains("Things to avoid"));
    }

    #[test]
    fn avoid_whitespace_only_strips_block() {
        let out = render(SAMPLE, "n", "tz", "r", "neutral", Some("   \n  "));
        assert!(!out.contains("Things to avoid"));
    }

    #[test]
    fn avoid_set_keeps_block_and_substitutes() {
        let out = render(SAMPLE, "n", "tz", "r", "neutral", Some("medical advice"));
        assert!(out.contains("Things to avoid: medical advice"));
        assert!(!out.contains("{{#if"));
        assert!(!out.contains("{{/if"));
    }

    #[test]
    fn substitutes_all_simple_placeholders() {
        let tpl = "{{name}}/{{timezone}}/{{role}}/{{tone}}";
        let out = render(tpl, "Shiro", "Asia/Jakarta", "build", "neutral", None);
        assert_eq!(out, "Shiro/Asia/Jakarta/build/neutral");
    }

    const TZ_SAMPLE: &str = "for {{name}}{{#if timezone}} (timezone: {{timezone}}){{/if}}.";

    #[test]
    fn empty_timezone_strips_timezone_block() {
        let out = render(TZ_SAMPLE, "Shiro", "", "build", "neutral", None);
        assert_eq!(out, "for Shiro.");
    }

    #[test]
    fn nonempty_timezone_keeps_timezone_block() {
        let out = render(TZ_SAMPLE, "Shiro", "Asia/Jakarta", "build", "neutral", None);
        assert_eq!(out, "for Shiro (timezone: Asia/Jakarta).");
    }

    /// Stripping the timezone block must not touch a sibling `{{#if avoid}}`
    /// block elsewhere in the template, and vice versa — each variable's
    /// guard is scoped to its own opener/closer pair.
    #[test]
    fn stripping_timezone_block_leaves_a_kept_avoid_block_intact() {
        let tpl = "for {{name}}{{#if timezone}} (timezone: {{timezone}}){{/if}}.\n\n{{#if avoid}}\nAvoid: {{avoid}}\n{{/if}}\n";
        let out = render(tpl, "Shiro", "", "build", "neutral", Some("small talk"));
        assert_eq!(out, "for Shiro.\n\nAvoid: small talk\n");
    }
}
