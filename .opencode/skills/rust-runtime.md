# Rust Runtime Skill

> **`CLAUDE.md` governs.** This file configures one agent harness; the repository's
> engineering protocol lives in `CLAUDE.md`. Where the two disagree, `CLAUDE.md` wins
> and this file is the thing that is wrong.

For Rust changes:

- Prefer explicit types and straightforward control flow.
- Keep async cancellation and shutdown paths clear.
- Avoid panics in runtime paths.
- Use structured errors.
- Preserve deterministic tests.
- Avoid heavy dependencies unless strongly justified.
- Respect binary size and release-profile goals.
- Prefer focused tests before broad test runs.
- Keep public CLI/config behavior stable unless explicitly changing it.

Before final answer, check whether the changed code needs:

- `cargo fmt --all -- --check`
- `cargo check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`
