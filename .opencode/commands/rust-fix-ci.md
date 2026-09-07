---
description: Fix failing Rust CI checks with minimal patch.
agent: builder
---

> **`CLAUDE.md` governs.** This file configures one agent harness; the repository's
> engineering protocol lives in `CLAUDE.md`. Where the two disagree, `CLAUDE.md` wins
> and this file is the thing that is wrong.

Use the failing output.

Process:

1. Identify exact failing command.
2. Locate root cause.
3. Patch the smallest relevant code.
4. Re-run the failing command.
5. Run one broader check if fixed.

Preferred commands:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
- `./dev/ci.sh all` when needed

Do not rewrite unrelated modules.

Final response:

1. Failing command
2. Root cause
3. Files changed
4. Validation result
5. Remaining risk
