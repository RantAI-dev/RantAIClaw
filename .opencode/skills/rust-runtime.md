# Rust Runtime Skill

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