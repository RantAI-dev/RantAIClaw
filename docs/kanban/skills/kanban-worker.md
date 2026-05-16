# `kanban-worker` skill

When the dispatcher spawns you with `RANTAICLAW_KANBAN_TASK=<id>` in your env, you are a kanban worker. Drive the task through the `kanban_*` tool surface — never shell out to `rantaiclaw kanban`.

## The contract

1. **On spawn**, call `kanban_show()` to read your task. The reply includes the title, body, parent handoffs (most recent completed run's summary + metadata per parent), prior attempts on this task, full comment thread, and a pre-formatted `worker_context` string you can paste into your reasoning.

2. **Heartbeat every few minutes** during long operations:
   ```
   kanban_heartbeat(note="halfway through — 4 of 8 files transformed")
   ```
   This extends your claim TTL. The dispatcher reclaims claims that go silent past `claim_ttl_seconds` (default 15 min).

3. **Finish with structured handoff**:
   ```
   kanban_complete(
       summary="implemented token bucket; keys on user_id with IP fallback; all tests pass",
       metadata={
           "changed_files": ["limiter.rs", "tests/test_limiter.rs"],
           "verification": ["cargo test --lib limiter"],
           "tests_run": 14,
           "residual_risk": [],
       },
   )
   ```
   `summary` is the human-readable closeout; `metadata` is the machine-readable handoff downstream agents and reviewers see.

4. **If you can't finish**, block:
   ```
   kanban_block(reason="need decision: should expired tokens 401 or 403?")
   ```
   A human (or an orchestrator) will `kanban_unblock` you once the question is answered. The next attempt's `kanban_show()` includes your block reason and any comments added since.

## Recommended metadata shape

```json
{
    "changed_files": ["path/to/file.rs"],
    "verification": ["cargo test --lib subject"],
    "dependencies": ["t_parent_id", "external issue id"],
    "blocked_reason": null,
    "retry_notes": "what failed before, if this was a retry",
    "residual_risk": ["what was not tested or still needs human review"]
}
```

These keys are a convention, not a schema requirement. The useful property is that every worker leaves enough evidence for the next reader to answer four questions quickly:

1. What changed?
2. How was it verified?
3. What can unblock or retry this if it fails?
4. What risk is still deliberately left open?

Keep secrets, raw logs, tokens, OAuth material, and unrelated transcripts out of `metadata`. Store pointers and summaries instead.

## Anti-patterns

- **Don't shell out to `rantaiclaw kanban complete`**. The CLI doesn't see the worker's claim, and `--metadata '{...}'` quoting is fragile. Use `kanban_complete()`.
- **Don't fan out into sibling tasks**. That's an orchestrator move — workers stay scoped to their own task. If the task description turned out to be wrong scope, `kanban_block` with the reason and let the orchestrator decompose.
- **Don't reassign your task**. If you can't do the work, `kanban_block`.
- **Don't lie about changed_files**. Listing files you didn't touch poisons downstream review. If you're unsure what changed, run `git status` first.
