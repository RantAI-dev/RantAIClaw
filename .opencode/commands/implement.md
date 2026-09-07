---
description: Implement a planned RantAIClaw change.
agent: builder
---

> **`CLAUDE.md` governs.** This file configures one agent harness; the repository's
> engineering protocol lives in `CLAUDE.md`. Where the two disagree, `CLAUDE.md` wins
> and this file is the thing that is wrong.

Follow `CLAUDE.md` and `AGENTS.md`.

Before editing:

- Restate the scope.
- Identify risk tier.
- Identify touched subsystem.
- Keep the patch minimal and reversible.

During implementation:

- Prefer existing traits and factories.
- Avoid cross-subsystem rewrites.
- Avoid new dependencies unless explicitly justified.
- Add or update focused tests when behavior changes.

After editing:

- Run the narrowest relevant validation first.
- Prefer:
  - `cargo fmt --all -- --check`
  - `cargo check`
  - `cargo test <focused>`
- For broader changes, run:
  - `cargo clippy --all-targets -- -D warnings`
  - `cargo test`

Final response:

1. Summary
2. Files changed
3. Validation run
4. Risks / rollback notes
5. Suggested next step
