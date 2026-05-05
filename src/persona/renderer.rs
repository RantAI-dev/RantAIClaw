//! Persona template renderer — substring substitution + `{{#if avoid}}` block guard.
//!
//! This is intentionally not a templating engine. The 5 bundled persona
//! markdown templates have a fixed, hand-curated set of placeholders
//! (`{{name}}`, `{{timezone}}`, `{{role}}`, `{{tone}}`, `{{avoid}}`) plus a
//! single `{{#if avoid}}...{{/if}}` block guard. A pure-string approach is
//! easier to audit, has no run-time dependency surface, and produces
//! deterministic output that round-trips through snapshot tests.
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
    // First decide whether the avoid block should survive.
    let keep_avoid = avoid.map(|s| !s.trim().is_empty()).unwrap_or(false);

    let stripped = if keep_avoid {
        // Keep the inner content, drop just the guard markers.
        template
            .replace("{{#if avoid}}\n", "")
            .replace("{{#if avoid}}", "")
            .replace("{{/if}}\n", "")
            .replace("{{/if}}", "")
    } else {
        strip_avoid_block(template)
    };

    let avoid_value = avoid.unwrap_or("");
    stripped
        .replace("{{name}}", name)
        .replace("{{timezone}}", timezone)
        .replace("{{role}}", role)
        .replace("{{tone}}", tone)
        .replace("{{avoid}}", avoid_value)
}

/// Remove `{{#if avoid}}...{{/if}}` (and its trailing blank line, if any) from
/// the template. Operates on raw source text — no regex dependency.
fn strip_avoid_block(template: &str) -> String {
    const OPEN: &str = "{{#if avoid}}";
    const CLOSE: &str = "{{/if}}";

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open_idx) = rest.find(OPEN) {
        out.push_str(&rest[..open_idx]);
        let after_open = &rest[open_idx + OPEN.len()..];
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
            out.push_str(OPEN);
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
}
