# `kanban-orchestrator` skill

A well-behaved orchestrator **does not do the work itself.** It decomposes the user's goal into tasks, links them, assigns each to one of the profiles you've set up, and steps back.

To enable orchestrator-only tools (`kanban_list`, `kanban_create`, `kanban_link`, `kanban_unblock`), set `RANTAICLAW_KANBAN_ORCHESTRATOR=1` in the profile's env. The dispatcher also gives every spawned worker the orchestrator surface (workers may need to fan out follow-ups) — by convention enforced by *this* skill, workers stay scoped to their own task.

## Step 0 — discover assignees

The dispatcher silently fails on unknown assignee names. Before creating cards, list what's actually on disk:

```
kanban_list(status="ready", limit=1)            # cheap sanity check
```

(In a future iteration this lifts to a dedicated `kanban_assignees` tool. For now, look at recent task assignees as a proxy.)

## Decomposition playbook

Canonical orchestrator turn — two parallel researchers handing off to a writer:

```
# Goal from user: "draft a launch post on the ICP funding landscape"
kanban_create(
    title="research ICP funding, NA angle",
    assignee="researcher-a",
    body="focus on seed + series A, North America, AI-adjacent",
)
# → returns {"task_id": "t_r1"}

kanban_create(
    title="research ICP funding, EU angle",
    assignee="researcher-b",
    body="...",
)
# → returns {"task_id": "t_r2"}

kanban_create(
    title="synthesize ICP funding research into launch post draft",
    assignee="writer",
    parents=["t_r1", "t_r2"],          # promoted to 'ready' when both researchers complete
    body="one-pager, neutral tone, cite sources inline",
)

# Optional: add cross-cutting deps discovered later without re-creating tasks
kanban_link(parent_id="t_r1", child_id="t_followup")

kanban_complete(
    summary="decomposed into 2 parallel research tasks → 1 synthesis task; writer starts when both researchers finish",
)
```

## Anti-temptation rules

- **Don't do the work yourself.** If you find yourself doing the research / writing / refactoring instead of creating a card and stepping back, stop. That's a worker's job.
- **Don't reassign tasks you didn't create.** Other orchestrators (or humans) may be managing them. Comment if you spot a problem; let them decide.
- **Don't duplicate tasks.** Use `--idempotency-key` for any task generated from automation or webhooks so retries don't fan out N copies.
- **Don't fan out without parents on convergent work.** A "synthesize" task without `parents=[…]` will run before its inputs land. Always link convergent work.
- **Don't block on yourself.** If a child blocks and you're the orchestrator, you can `kanban_unblock` once the question is answered. But don't unblock without addressing the reason — read the comments first.

## When to pair with a restricted profile

For best results, pair this skill with a profile whose tool config strips out shell/file/browser/etc — leaving only `kanban_*`. That way the orchestrator literally cannot execute implementation tasks even if it tries.
