# Plan 196: Make `is_args_safe` quote-aware so `find . '-exec'` can't smuggle a dangerous arg past the gate

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If a "STOP condition" occurs, stop and report — do not improvise.
> When done, update this plan's status row in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat bba8e1d..HEAD -- src/security/policy.rs`
> If `policy.rs` changed since this plan was written, compare the "Current
> state" excerpts against the live code before proceeding; on a mismatch,
> treat it as a STOP condition.

## Status

- **Priority**: P0 (security — command-execution bypass)
- **Effort**: M
- **Risk**: MED
- **Depends on**: none
- **Category**: security / bug
- **Planned at**: commit `bba8e1d`, 2026-08-20

## Why this matters

The shell command gate decides whether an allowlisted command runs, and one
of its layers is `is_args_safe`, which blocks dangerous **arguments** on
otherwise-allowlisted binaries (`find … -exec`, `git … --upload-pack=`, etc.).

The whole final command string is executed via `/bin/sh -c "<command>"`
(`src/runtime/native.rs:62-63`), so the shell performs quote removal and
expansion. But the policy tokenizes with `split_whitespace()` and compares the
**raw, quote-bearing** tokens against its blocklists. A quote inserted inside a
flag defeats every one of these literal comparisons while the shell still sees
the un-quoted flag:

- `find . '-exec' rm -rf {} +` — the policy token is `'-exec'`, which is not
  equal to `-exec`, so `is_args_safe` returns `true`. `find` is neither medium
  nor high risk, so `command_risk_level` returns `Low` → **no approval prompt**
  → the shell runs `find … -exec rm`.
- `git ls-remote --upload-pac"k"=/tmp/evil .` — the token still contains a
  quote, so neither the exact-string check nor `is_dangerous_git_long_opt`
  matches; the shell strips the quote and launches the transport program.

The `-exec` case is the sharp one: it is classified `Low`, so it never even
prompts. This is exactly the "misclassification that skips a Supervised
approval" the code comment at `command_risk_level` warns about, but for
argument-safety rather than verb position.

Separately (`is_dangerous_git_long_opt`), the abbreviation guard has a
`name.len() < 4` floor, but git accepts 3-char unambiguous abbreviations
(`--exe` → `--exec`, `--upl` → `--upload-pack`), leaving a 3-char gap.

## Current state

### `is_command_allowed` tokenizes raw, then lowercases — but never de-quotes — `src/security/policy.rs:783-810`

```rust
        let segments = split_unquoted_segments(command);
        for segment in &segments {
            let cmd_part = skip_env_assignments(segment);
            let mut words = cmd_part.split_whitespace();
            let base_raw = words.next().unwrap_or("");
            let base_cmd = base_raw.rsplit('/').next().unwrap_or("");
            ...
            // Validate arguments for the command
            let args: Vec<String> = words.map(|w| w.to_ascii_lowercase()).collect();
            if !self.is_args_safe(base_cmd, &args) {
                return false;
            }
```

`args` are lowercased but **quotes are retained**, so `'-exec'` stays
`'-exec'`.

### `is_args_safe` does literal equality/prefix matching — `src/security/policy.rs:821-870`

```rust
    fn is_args_safe(&self, base: &str, args: &[String]) -> bool {
        let base = base.to_ascii_lowercase();
        match base.as_str() {
            "find" => {
                const FIND_DANGEROUS: &[&str] = &[
                    "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0",
                    "-fprintf", "-fls",
                ];
                !args.iter().any(|arg| FIND_DANGEROUS.contains(&arg.as_str()))
            }
            "git" => { /* exact `==`/`starts_with` checks + is_dangerous_git_long_opt */ }
            _ => true,
        }
    }
```

`command_risk_level` (`policy.rs:476-593`) tokenizes the same way, so the same
quote trick keeps `find … '-exec'` classified `Low`.

### The abbreviation floor — `src/security/policy.rs:455-467`

```rust
fn is_dangerous_git_long_opt(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix("--") else { return false; };
    let name = rest.split('=').next().unwrap_or(rest);
    if name.len() < 4 { return false; }   // 3-char abbrevs slip through
    ["upload-pack", "receive-pack"].iter().any(|full| full.starts_with(name))
        || "exec".starts_with(name)
}
```

### The quoting machinery already exists — `src/security/policy.rs:245-347`

`split_unquoted_segments` already tracks `QuoteState` (Single/Double/None)
while splitting on `;`/`|`/`&`. The fix reuses this same quote-awareness to
strip quotes from each token before the safety checks.

## The fix

Introduce a small helper that turns a raw command segment into the argv the
shell would actually see (quotes removed), and feed **that** argv to both
`is_args_safe` and the arg scan in `command_risk_level`.

### Step 1 — add a quote-stripping tokenizer

In `src/security/policy.rs`, add a private free function near
`split_unquoted_segments`:

```rust
/// Split one already-separator-free command segment into the argv the shell
/// (`sh -c`) would see: whitespace-separated, with unescaped `'…'`/`"…"`
/// quotes removed so `'-exec'` becomes `-exec`. This models only what the
/// safety checks need — quote removal — not full shell expansion.
fn shell_argv(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote = QuoteState::None;
    let mut chars = segment.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            QuoteState::Single => {
                if c == '\'' { quote = QuoteState::None; } else { cur.push(c); }
            }
            QuoteState::Double => {
                if c == '"' { quote = QuoteState::None; }
                else if c == '\\' {
                    if let Some(&n) = chars.peek() {
                        if n == '"' || n == '\\' || n == '$' || n == '`' { cur.push(n); chars.next(); }
                        else { cur.push('\\'); }
                    } else { cur.push('\\'); }
                } else { cur.push(c); }
            }
            QuoteState::None => match c {
                '\'' => { quote = QuoteState::Single; in_word = true; }
                '"'  => { quote = QuoteState::Double; in_word = true; }
                '\\' => { if let Some(n) = chars.next() { cur.push(n); in_word = true; } }
                c if c.is_whitespace() => {
                    if in_word { out.push(std::mem::take(&mut cur)); in_word = false; }
                }
                _ => { cur.push(c); in_word = true; }
            },
        }
    }
    if in_word || !cur.is_empty() { out.push(cur); }
    out
}
```

### Step 2 — feed de-quoted argv to the safety checks

In `is_command_allowed` (`policy.rs:785`), replace the `split_whitespace()`
tokenization with `shell_argv(cmd_part)`: take the first element as the base,
the rest (lowercased) as `args`. Keep the existing `rsplit('/')` basename
reduction on the base.

In `command_risk_level` (`policy.rs:481-491`), do the same: build the base and
`args` from `shell_argv(cmd_part)` instead of `split_whitespace()`.

Preserve the existing behavior for unquoted input — `shell_argv("find . -name '*.txt'")`
must yield `["find", ".", "-name", "*.txt"]` (quotes gone, glob char kept).

### Step 3 — close the abbreviation floor

In `is_dangerous_git_long_opt` (`policy.rs:460`), lower the floor from `< 4`
to `< 3` so `--exe`/`--upl` are caught. Keep the ambiguity note in a comment;
the transport verbs stay Medium-gated as defense in depth.

## Files

- **In scope**: `src/security/policy.rs` only.
- **Out of scope**: `src/tools/shell.rs`, `src/runtime/native.rs`, the risk
  verb lists (that is plan 200), forbidden-path handling (plan 198). Do **not**
  attempt full shell parsing (variable/`$()`/glob expansion) — quote removal is
  the exact and sufficient scope here.

## STOP conditions

- If `shell_argv` changes the parse of any existing legitimate-command test
  (e.g. `git status`, `find . -name '*.txt'`) so it now fails the allowlist,
  STOP — the tokenizer regressed; do not "fix" by loosening the blocklists.
- If a quote-aware tokenizer already exists in `policy.rs` (search for a fn
  that removes quotes), use it instead of adding a duplicate, and report.

## Done criteria

1. `cargo fmt --all -- --check` clean.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `cargo test -p rantaiclaw --lib security::policy` passes, including the new
   tests below.
4. New tests prove the bypass is closed (all must FAIL before the Step 2 edit
   and PASS after — verify by temporarily reverting Step 2):

```rust
#[test]
fn quoted_find_exec_is_still_blocked() {
    let p = /* Supervised policy with `find` allowlisted, as existing find tests build it */;
    assert!(!p.is_command_allowed("find . '-exec' rm -rf {} +"));
    // and it is not Low-risk (so a Supervised session would gate it if allowed)
    assert_ne!(p.command_risk_level("find . '-exec' rm {} +"), CommandRiskLevel::Low);
}

#[test]
fn quoted_git_upload_pack_is_still_blocked() {
    let p = /* policy with `git` allowlisted */;
    assert!(!p.is_command_allowed(r#"git ls-remote --upload-pac"k"=/tmp/x ."#));
}

#[test]
fn git_short_abbrev_exec_is_blocked() {
    let p = /* policy with `git` allowlisted */;
    assert!(!p.is_command_allowed("git push --exe=/tmp/x"));
}

#[test]
fn legitimate_quoted_find_still_allowed() {
    let p = /* policy with `find` allowlisted */;
    assert!(p.is_command_allowed("find . -name '*.txt'"));
}
```

## Test plan

Add the four tests above to the existing `#[cfg(test)]` module in
`src/security/policy.rs` (find the module that already builds a Supervised
policy with `find`/`git` allowlisted and mirror its fixture). The first three
are the regression proofs; the fourth is the no-regression guard for
legitimate quoted input.

## Risk & rollback

- **Risk**: MED — the tokenizer sits on the shell hot path; a parsing
  regression could over-block legitimate commands. The no-regression test and
  the STOP condition guard against that.
- **Rollback**: single-file revert of the commit; no schema/config/API change,
  no migration.

## Maintenance note

Any future dangerous-arg blocklist added to `is_args_safe` (new subcommands,
new tools) automatically inherits the de-quoting once Step 2 lands, because it
consumes the same `args`. Reviewers adding such a list should assert a quoted
variant in tests.
