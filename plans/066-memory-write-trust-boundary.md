# 066 — Nothing screens what becomes a memory

- **Findings:** #17, #29 (memory deepscan, wave 4 — independent lane)
- **Written against:** `e298f3d`
- **Risk tier:** **high** (`src/tools/**`, trust boundary)
- **Effort:** M
- **Depends on:** nothing
- **Blocks:** nothing

## Problem A — memory writes are unvalidated (#17)

`memory_store` performs no content validation, and channel auto-save stores raw
inbound messages verbatim.

Memory is the one store whose contents are read back into a prompt on a later turn, in a
later session, with nobody looking at them again. That makes a write the *durable* end of
an injection: text that survives here is re-presented to the model as established fact for
as long as it stays stored. Auto-save writes raw user text, so this is not hypothetical.

The only existing filter is `is_assistant_autosave_key`, which skips a legacy key prefix
on the read side. That predicate exists because auto-saved model prose was once
re-injected as fact — evidence the failure mode is real here, not theoretical.

## Problem B — memory content reaches a CLI as argv (#29)

`LucidMemory::build_store_args` places `"{key}: {content}"` as a positional argument.
Content beginning with `-` is read by the receiving CLI as a flag. The content is agent-
and user-influenced, which makes it argument injection rather than a formatting quirk. Not
command injection — `Command::new` takes an array and no shell is involved.

## Change

### Files in scope

- `src/memory/sanitize.rs` (new) — the screening rules
- `src/memory/mod.rs` — register and re-export
- `src/tools/memory_store.rs` — screen agent-initiated writes
- `src/channels/mod.rs` — screen auto-saved messages
- `src/memory/lucid.rs` — `--` before positional arguments

### Files explicitly out of scope

- Read-side filtering — `memory::context` already owns which entries reach a prompt
- A general content filter. This rejects what can *forge structure* or *leak
  credentials*, not what looks suspicious. Everything else is a false positive waiting
  to refuse a legitimate fact.

### The three rules

1. **Strip invisible characters.** Zero-width joiners, bidi overrides, control
   characters. They carry no meaning in a stored fact and are how one instruction hides
   inside another that reads innocently. `\n`, `\t`, `\r` are layout and stay.

2. **Refuse content carrying `[Memory context]`.** That is the header the injected block
   opens with; content containing it could close the real block and open a forged one, so
   a single stored memory could impersonate several. Checked *after* stripping, so an
   invisible character cannot smuggle the marker past it.

3. **Redact credential-shaped tokens** through the existing `scrub_secret_patterns`
   rather than a second pattern list. A stored token is re-injected into every prompt
   that recalls it and travels to the provider each time. Reusing the project's list is
   the point: two lists drift, and this deepscan has already found that failure twice.

Rules 1 and 3 adjust and report; rule 2 refuses. The difference is whether the content
can be stored safely at all.

## Verification

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --lib memory::
cargo test --lib tools::
cargo test --lib channels::
```

## Test plan

Unit, in `sanitize.rs`: ordinary content untouched, newlines survive, invisible
characters stripped, the marker refused, the marker refused *through* zero-width padding,
credentials redacted.

Integration, in `memory_store.rs`: forgery refused **and nothing stored**, credential
redacted with the change reported, invisible characters stripped.

Each checked against the pre-change path by bypassing the screen.

## Escape hatches

- If the marker refusal starts rejecting legitimate memories in practice, STOP and
  report. Narrowing it to "at the start of a line" is a different tradeoff and should be
  a decision, not a quiet edit.
- If `scrub_secret_patterns` proves too aggressive on ordinary prose, STOP — widening or
  narrowing that list affects provider error scrubbing too, and belongs to whoever owns
  it.

## Maintenance note

Every path that writes memory must go through `sanitize_memory_content`. There are two
today: the tool and channel auto-save. A third added later that skips it reopens this,
and nothing in the type system will say so.

## Rollback

`git revert` removes the screen and the `--` separator. Already-stored content is
unaffected either way; nothing is rewritten retroactively.
