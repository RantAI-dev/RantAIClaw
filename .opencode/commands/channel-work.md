---
description: Add or modify a RantAIClaw channel.
agent: architect
---

> **`CLAUDE.md` governs.** This file configures one agent harness; the repository's
> engineering protocol lives in `CLAUDE.md`. Where the two disagree, `CLAUDE.md` wins
> and this file is the thing that is wrong.

Follow channel playbook:

- Inspect `src/channels/traits.rs`
- Inspect similar channel implementation
- Preserve send/listen/health semantics
- Check auth/allowlist behavior
- Check gateway/security interaction
- Plan tests for lifecycle and failure modes
- Check docs impact

Return a plan first.

Do not edit unless explicitly asked after planning.
