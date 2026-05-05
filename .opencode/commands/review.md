---
description: Review current diff for RantAIClaw correctness and architecture risk.
agent: reviewer
---

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