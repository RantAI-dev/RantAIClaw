# Project — conventions + archive

This directory holds two things:

1. **Operating conventions** — how alpha cuts ship, validate, and
   tag. See [`operating-conventions.md`](operating-conventions.md).
   Read this before touching code if you're new to the repo or
   coming back after a gap.
2. **Archive** — superseded plans, gap trackers, and snapshot audits.
   See [`archive/`](archive/). Old plans live here for design-
   rationale history; do not edit them. Active planning lives in
   ClickUp ([v0.6.0 milestone](https://app.clickup.com/t/86exgu406)
   and its successors).

## Scope

Snapshots are time-bound — they go stale the moment they're written.
Once a plan ships or is superseded, archive it rather than try to
keep it current. The single source of truth for "what's planned right
now" is the ClickUp board.

For stable runtime-contract docs (commands / config / providers /
channels) see [`../`](../). Those track behavior changes on every PR;
they don't go stale.
