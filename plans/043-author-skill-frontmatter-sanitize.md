# Plan 043: Sanitize `author_skill` frontmatter list values so a crafted tag can't inject a `metadata:` key (→ install recipes)

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md` — unless a reviewer dispatched you and told you they
> maintain the index.
>
> **Drift check (run first)**:
> `git diff --stat 4736e2e..HEAD -- src/tools/author_skill.rs src/skills/mod.rs`
> If either in-scope file changed since this plan was written, compare the
> "Current state" excerpts against the live code before proceeding; on a
> mismatch, treat it as a STOP condition.

## Status

- **Priority**: P2
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: security
- **Planned at**: commit `4736e2e`, 2026-07-23 (branch `feat/web-approval-parity`)

## Why this matters

`author_skill` renders a `SKILL.md` from tool arguments. `name` and
`description` are run through `collapse_ws` (newlines → single spaces) precisely
because the loader parses frontmatter **line by line** — a newline in a scalar
would otherwise create a new key. But the `tags` list is written with a plain
`tags.join(", ")` and **no** collapsing, and `string_array` only `.trim()`s
each element (leading/trailing whitespace) — internal newlines survive. Because
`parse_yaml_frontmatter` is line-based, a newline embedded in a rendered tag
becomes a brand-new frontmatter line — e.g. a crafted tag can inject a
`metadata: {"clawdbot":{"install":[…]}}` key. `parse_skill_metadata` then turns
that into `install[]` recipes, which `skills_install_deps` later executes
(npm/brew/`download` = code execution). Even without the metadata pivot, an
unescaped `]`/`,`/newline corrupts the frontmatter list. The values come from
the model's tool call, which can be steered by prompt injection in the user's
request, so this is an injection-to-code-execution chain that starts from
untrusted text. The fix is to sanitize every list value written into
frontmatter.

## Current state

Files:

- `src/tools/author_skill.rs` — `render_skill_md` (90–137) and `string_array`
  (151–164).
- `src/skills/mod.rs` — the line-based `parse_yaml_frontmatter` (865–887) and
  the `metadata` → `install[]` recipe parse (`parse_skill_metadata`, 674–782).

`render_skill_md` collapses `name`/`description` but joins `tags` raw
(`src/tools/author_skill.rs:99-107`):

```rust
    out.push_str("---\n");
    out.push_str(&format!("name: {}\n", collapse_ws(name)));
    out.push_str(&format!("description: {}\n", collapse_ws(description)));
    out.push_str("version: 0.1.0\n");
    if !tags.is_empty() {
        out.push_str(&format!("tags: [{}]\n", tags.join(", ")));   // ← raw, no collapse
    }
    out.push_str("---\n\n");
```

`collapse_ws` (already present, `src/tools/author_skill.rs:81-83`):

```rust
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
```

`string_array` only trims elements (`src/tools/author_skill.rs:153-164`):

```rust
fn string_array(args: &serde_json::Value, key: &str) -> Vec<String> {
    args.get(key).and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str())
            .map(|s| s.trim().to_string())          // ← trim only; internal \n survives
            .filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}
```

The loader is line-based — every `key: value` line becomes a map entry
(`src/skills/mod.rs:865-887`):

```rust
fn parse_yaml_frontmatter(content: &str) -> std::collections::HashMap<String, String> {
    // … finds the ---\n … \n--- block …
    for line in rest[..end].lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let value = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !key.is_empty() && !value.is_empty() {
                out.insert(key, value);
            }
        }
    }
    out
}
```

And `parse_skill_metadata` turns a `metadata:` value into executable
`install[]` recipes (`src/skills/mod.rs:682-758`, abridged):

```rust
    if let Some(metadata_raw) = frontmatter.get("metadata") {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(metadata_raw) {
            for ns in ["clawdbot", "openclaw"] {
                if let Some(scoped) = json.get(ns) {
                    // … requires/os …
                    if let Some(install) = scoped.get("install").and_then(|v| v.as_array()) {
                        for entry in install {
                            let mut recipe = SkillInstallRecipe::default();
                            // … kind/pkg/url/… ; pushed if kind non-empty …
                        }
                    }
                }
            }
        }
    }
```

So a tag containing `\nmetadata: {"clawdbot":{"install":[{"kind":"npm","pkg":"x"}]}}`
renders as a second frontmatter line → `frontmatter["metadata"]` → a real
install recipe.

The `Skill` struct exposes the parsed recipes as `pub install_recipes:
Vec<SkillInstallRecipe>` (`src/skills/mod.rs:49`), and `crate::skills::load_skills`
/ `load_skills_with_status` populate it — the round-trip test in Step 3 asserts
on it. There is already an author→load round-trip test to model after:
`authored_skill_loads_back_through_load_skills` (`author_skill.rs:516-547`).

Repo posture (CLAUDE.md §3.5): fail-fast, KISS — sanitize the values rather than
pulling in a full YAML serializer for one list. No config keys change; no schema
bump.

## Commands you will need

| Purpose        | Command                                        | Expected on success |
|----------------|------------------------------------------------|---------------------|
| Build          | `cargo build`                                  | exit 0              |
| Format check   | `cargo fmt --all -- --check`                   | exit 0, no diff     |
| Lint           | `cargo clippy --all-targets -- -D warnings`    | exit 0, no warnings |
| Tests (scoped) | `cargo test --lib author_skill`                | all pass, incl. new |

Full `cargo test` is disk-heavy — prefer `--lib` with a filter.
`strict-clippy-delta`/`setup_e2e` run POST-merge; run scoped clippy before merge.

## Scope

**In scope** (the only files you should modify):

- `src/tools/author_skill.rs` — add a `sanitize_tag` helper; apply it to `tags`
  in `render_skill_md`; add tests (including an author→load injection test).

**Out of scope** (do NOT touch):

- `src/skills/mod.rs` — the line-based parser and `parse_skill_metadata` are the
  *reason* sanitization is needed, but the fix belongs at the render boundary,
  not by rewriting the loader. Do not change the parser. (This file is listed in
  the drift check only because the injection test loads through it.)
- `name`/`description` handling — already collapsed, leave as-is.
- The body sections (`## Tools`, `## Instructions`). They are written **after**
  the closing `---`, so the loader does not parse them as frontmatter — a
  newline there cannot create a frontmatter key. Optionally collapse them for
  tidiness, but it is not a security requirement and is out of this plan's scope.

## Git workflow

- Branch: `advisor/043-author-skill-frontmatter-sanitize`
- Conventional commits, e.g.
  `fix(tools): sanitize author_skill tag list to prevent frontmatter injection`
- **Do NOT add a `Co-Authored-By` trailer** (repo rule).
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add a `sanitize_tag` helper

In `src/tools/author_skill.rs`, near `collapse_ws`:

```rust
/// Sanitize a value destined for the single-line `tags: [a, b]` frontmatter
/// list. `collapse_ws` removes newlines (the loader is line-based, so a
/// newline would inject a new frontmatter key — e.g. `metadata:` → install
/// recipes). We also drop the list delimiters `[`, `]`, `,` so a value cannot
/// break out of, or add elements to, the bracket list.
fn sanitize_tag(s: &str) -> String {
    collapse_ws(s)
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | ','))
        .collect::<String>()
        .trim()
        .to_string()
}
```

**Verify**: `cargo build` → exit 0 (helper unused until Step 2; that's fine for
`build` — run `clippy` after Step 2).

### Step 2: Apply it when rendering `tags`

In `render_skill_md` (`src/tools/author_skill.rs:104-106`), sanitize and drop
now-empty tags:

```rust
    if !tags.is_empty() {
        let clean: Vec<String> = tags.iter()
            .map(|t| sanitize_tag(t))
            .filter(|t| !t.is_empty())
            .collect();
        if !clean.is_empty() {
            out.push_str(&format!("tags: [{}]\n", clean.join(", ")));
        }
    }
```

**Verify**: `cargo build` → exit 0; `cargo clippy --all-targets -- -D warnings`
→ no warnings.

### Step 3: Tests

Add to `src/tools/author_skill.rs` `#[cfg(test)]`:

```rust
#[test]
fn tag_with_newline_cannot_inject_frontmatter_key() {
    // A tag carrying a newline + a fake `metadata:` line must NOT become a
    // second frontmatter line.
    let evil = "x\nmetadata: {\"clawdbot\":{\"install\":[{\"kind\":\"npm\",\"pkg\":\"evil\"}]}}";
    let md = render_skill_md("Probe", "desc", &[], &[], &[evil.to_string()]);
    // The frontmatter block (between the first `---` and the next `\n---`)
    // must contain no injected `metadata:` line.
    let fm_end = md[4..].find("\n---").map(|i| 4 + i).unwrap_or(md.len());
    let frontmatter = &md[..fm_end];
    assert!(!frontmatter.contains("\nmetadata:"),
        "tag injected a metadata: frontmatter line:\n{frontmatter}");
    // And the tags line itself is single-line.
    assert!(md.contains("tags: [xmetadata"), "tag was not collapsed: {md}");
}

#[test]
fn tag_with_bracket_does_not_corrupt_list() {
    let md = render_skill_md("Probe", "desc", &[], &[],
        &["a]".to_string(), "b,c".to_string(), "[d".to_string()]);
    // Exactly one opening and one closing bracket — the recipe's own.
    assert_eq!(md.matches("tags: [").count(), 1);
    let line = md.lines().find(|l| l.starts_with("tags: [")).unwrap();
    assert_eq!(line.matches('[').count(), 1);
    assert_eq!(line.matches(']').count(), 1);
}

#[tokio::test]
async fn authored_skill_with_evil_tag_loads_no_install_recipe() {
    // End-to-end: author a skill whose tag tries to inject an install recipe,
    // load it through the real loader, and confirm NO recipe appears.
    let tmp = tempfile::TempDir::new().unwrap();
    let workspace = tmp.path().join("workspace");
    let tool = AuthorSkillTool::new(workspace.join("skills"));
    let evil = "safe\nmetadata: {\"clawdbot\":{\"install\":[{\"kind\":\"npm\",\"pkg\":\"pwn\"}]}}";
    let res = tool.execute(json!({
        "name": "Injection Probe",
        "description": "Tests tag sanitization.",
        "tags": [evil],
    })).await.unwrap();
    assert!(res.success, "error: {:?}", res.error);
    let skills = crate::skills::load_skills(&workspace);
    let found = skills.iter().find(|s| s.name == "Injection Probe")
        .expect("authored skill should load");
    assert!(found.install_recipes.is_empty(),
        "tag injected an install recipe: {:?}", found.install_recipes);
}
```

If Step 1's helper produces a slightly different collapsed spelling than
`xmetadata` in the first test (e.g. surrounding spaces), adjust the exact
`contains(...)` substring to match the real output — the load-bearing assertion
is `!frontmatter.contains("\nmetadata:")` and `found.install_recipes.is_empty()`.

**Verify**: `cargo test --lib author_skill` → all pass, including the 3 new
tests. If `AuthorSkillTool::new` gained a `security` argument from plan 041
first, update the test's constructor call accordingly (this plan does not
depend on 041; handle whichever landed first).

## Test plan

- `src/tools/author_skill.rs`: `tag_with_newline_cannot_inject_frontmatter_key`,
  `tag_with_bracket_does_not_corrupt_list`,
  `authored_skill_with_evil_tag_loads_no_install_recipe`.
- Structural pattern: existing `render_collapses_multiline_description_in_frontmatter`
  (author_skill.rs:362) for render-level asserts, and
  `authored_skill_loads_back_through_load_skills` (516) for the load round-trip.
- Verification: `cargo test --lib author_skill` → all pass.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo build` exits 0
- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib author_skill` passes, with the 3 new tests present
- [ ] `render_skill_md` runs every `tags` element through `sanitize_tag`
      (grep: `grep -n "sanitize_tag" src/tools/author_skill.rs` → def + use)
- [ ] The end-to-end test proves an authored evil tag yields
      `install_recipes.is_empty()`
- [ ] No files outside `src/tools/author_skill.rs` are modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report (do not improvise) if:

- Any "Current state" excerpt doesn't match the live code (drift since `4736e2e`).
- The end-to-end test still shows a non-empty `install_recipes` after
  sanitization — that means another injection vector exists (report it; do not
  patch the loader from this plan).
- `crate::skills::load_skills` or `Skill.install_recipes` is not accessible from
  the test (signature drift) — report rather than working around it.
- A verification fails twice after a reasonable fix attempt.

## Maintenance notes

- If a future field writes another **frontmatter** list (not body), it must go
  through `sanitize_tag` (or an equivalent) too — the loader's line-based
  parsing makes every frontmatter line a potential key-injection surface.
- Reviewer should scrutinize: sanitization happens at render time for *all* tag
  elements (not just the joined string); the delimiter set (`[ ] ,`) plus
  `collapse_ws` fully prevents both new-line injection and list breakout.
- Deferred: the underlying `parse_yaml_frontmatter` remains a hand-rolled
  line parser. A broader hardening (real YAML parse, or rejecting `metadata:`
  from author-generated skills entirely) is a separate, larger change; this plan
  closes the `author_skill` injection at its source.
