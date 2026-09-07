---
description: Review current diff for RantAIClaw correctness and architecture risk.
agent: reviewer
---

> **`CLAUDE.md` governs.** This file configures one agent harness; the repository's
> engineering protocol lives in `CLAUDE.md`. Where the two disagree, `CLAUDE.md` wins
> and this file is the thing that is wrong.

Review `git diff`.

Check:

- Trait/factory boundary violations
- Security default weakening
- Config/schema compatibility
- CLI behavior changes
- Provider/channel/tool contract changes
- Async/concurrency mistakes
- Error handling quality
- New dependency weight
- Missing tests
- Missing docs updates
- Rollback difficulty
- Accidental unrelated changes

Do not edit files.

Return:

1. Blocking issues
2. Medium-risk issues
3. Nice-to-have improvements
4. Required validation
5. Verdict: safe / risky / needs changes
