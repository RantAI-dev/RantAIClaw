---
description: Add or modify a RantAIClaw provider.
agent: architect
---

> **`CLAUDE.md` governs.** This file configures one agent harness; the repository's
> engineering protocol lives in `CLAUDE.md`. Where the two disagree, `CLAUDE.md` wins
> and this file is the thing that is wrong.

Follow provider playbook:

- Inspect `src/providers/traits.rs`
- Inspect existing provider implementations
- Inspect provider factory registration
- Preserve shared orchestration boundaries
- Do not leak provider-specific behavior into agent loop unless justified
- Check config and docs impact

Return a plan first.

Do not edit unless explicitly asked after planning.
