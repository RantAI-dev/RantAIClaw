# 085 — `memory_forget` consults memory before its autonomy gate

Written against `7114f88`. Risk tier: **LOW** (`src/tools/**`, gate ordering).
No privilege escalation — see "What is and is not broken".

`MemoryForgetTool::execute` (`src/tools/memory_forget.rs:47-93`) resolves the
selector first and enforces the policy second:

```rust
let key: String = match (key_arg, contains_arg) {
    (None, Some(needle)) => resolve_unique_entry(self.memory.as_ref(), needle, "contains")  // reads all memory
    ...
};
if let Err(error) = self.security.enforce_tool_operation(ToolOperation::Act, "memory_forget") { ... }
```

`resolve_unique_entry` (`src/tools/memory_store.rs`) calls `memory.list(None, None)`
and, on an ambiguous match, returns an error naming the matching keys. So a
read-only or rate-limited caller using `contains` gets that answer instead of the
refusal:

```
---- tools::memory_forget::tests::probe_readonly_gate_applies_to_the_contains_selector ----
read-only call answered from memory contents instead of the gate:
'deploy' matches 2 memories (b, a); be more specific or address one by key
```

## What is and is not broken

**Not broken:** nothing is deleted. The probe's control asserted both entries
still present after the read-only call, and it passed. The gate does stop the
mutation; it just stops it late.

**Not an information leak in the escalation sense either.** `ReadOnly` permits
`ToolOperation::Read`, so `memory_recall` is available to the same caller and
returns memory content directly. The error message reveals nothing the caller
could not already ask for.

**What is actually wrong:**

1. A blocked caller is told "be more specific" — an instruction to retry — rather
   than "read-only mode". An agent following that message will loop on a call it
   can never complete.
2. The tool performs a full `list()` of memory on a call it has already decided,
   by policy, not to honour.
3. The rate-limit path is worse than the read-only one: `enforce_tool_operation`
   is what *records* the action, so on the `contains` path the work happens
   outside anything the limiter accounts for.
4. Both existing gate tests (`forget_blocked_in_readonly_mode:245`,
   `forget_blocked_when_rate_limited:263`) use the `key` selector, so this half of
   the surface has never been covered.

## Fix

Move the `enforce_tool_operation` block above the selector `match`. The gate
depends on nothing the match produces — it takes a fixed `ToolOperation::Act` and
the literal tool name — so this is a pure reordering.

Keep the `(None, None) => Err(...)` arm as an `anyhow::Err` rather than a
`ToolResult`: the current shape distinguishes "the model called this wrong"
(hard error) from "the call was well-formed but refused" (`ToolResult`), and
`forget_missing_key:165` asserts it. Reordering must not change that. Argument
shape is the model's mistake and should be reported whether or not the caller is
permitted to act — so validate the selector *pair* first (the three-way
`(Some,Some)` / `(None,None)` check), then gate, then resolve `contains`. That
ordering keeps every existing test's expectations intact.

Check `src/tools/memory_store.rs` for the same shape — it calls
`resolve_unique_entry` for its `replaces` selector and enforces at `:128`. If the
gate is below the resolve there too, fix both; they are the same defect and the
same commit.

## Non-goals

- Changing what `resolve_unique_entry` reveals on an ambiguous match. Naming the
  candidates is what makes the error actionable for a permitted caller, and
  `forget_by_ambiguous_contains_is_rejected:197` pins it.
- Reworking `ToolOperation` granularity so a resolve counts as a `Read`. YAGNI —
  no caller needs that distinction today.

## Validation

- Unit: `ReadOnly` + `{"contains": "..."}` matching two entries → error contains
  `"read-only mode"`, and both entries survive.
- Unit: `ReadOnly` + `{"contains": "..."}` matching exactly one entry → error
  contains `"read-only mode"`, and the entry survives. (This is the case where
  resolution *succeeds*, so it proves the gate moved rather than that ambiguity
  happens to shadow it.)
- Unit: `with_max_actions_per_hour(0)` + `contains` → `"Rate limit exceeded"`.
- Unit: the four existing tests at `:224`, `:245`, `:263`, `:165` must still pass
  unchanged — do not edit them to accommodate the new ordering. If one needs
  editing, the reordering went further than this plan authorises.
- `cargo test --lib -- memory_forget memory_store`

## Rollback

Single commit, one function body reordered (two if `memory_store.rs` shares the
defect). Revert directly.
