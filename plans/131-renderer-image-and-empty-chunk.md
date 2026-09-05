# Plan 131: Renderer — carry image URLs, never emit an empty chunk

> **Executor instructions**: Follow this plan step by step. Run every verification
> command and confirm the expected result before moving to the next step. If anything
> in the "STOP conditions" section occurs, stop and report — do not improvise. When
> done, update the status row for this plan in `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat f189422..HEAD -- src/channels/format/`
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged first.
> That is expected and is not a stop condition. Relocate by symbol name and continue.
> STOP only if the *code itself* no longer matches the "Current state" excerpt
> semantically — i.e. the logic changed, not its position.

## Status

- **Priority**: P2
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none (but see "Maintenance notes" — pairs with plan 129)
- **Category**: bug
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

`src/channels/format/` is the strongest code in the subsystem. All 2,071 production
lines were read in full during the audit and came back with **one real defect** and
one contract sharp edge. This plan fixes exactly those two and nothing else — the
module's accepted quirks are deliberate, documented, and out of scope.

The defect: the AST builder never matches `Tag::Image`, so both the image tag and its
close fall into a catch-all and the URL is discarded. Every outbound message on all
eighteen channels loses image links — `![chart](https://…/chart.png)` reaches the
user as the bare word `chart`. On Telegram that is a real regression against the
channel's own attachment path, which *does* upload images: a model emitting standard
markdown instead of the proprietary `[IMAGE:...]` marker silently loses the artifact.

The sharp edge: `split` guarantees at least one chunk, returning an empty string when
there is nothing to emit — and callers post it. Discord answers "cannot send an empty
message", `send()` bails, and the dispatch loop records a delivery failure for a turn
that had nothing to deliver. Reachable from whitespace-only content, an image-only
paragraph (the defect above), and a Telegram reply that is entirely a tool-call block.

## Current state

`src/channels/format/ast.rs:255-329` (`Builder::start`) and `:331-431`
(`Builder::end`) — `Tag::Image` and `TagEnd::Image` match no arm and fall into
`_ => {}`. Confirmed: `grep -c 'Tag::Image\|TagEnd::Image' src/channels/format/ast.rs`
returns **0**.

`Tag::Link` at `:323` pushes an inline frame and records `dest_url`; `Image` gets
neither, so the alt-text `Event::Text` between them lands in the enclosing run via
`push_inline` and the URL is dropped. `![](x.png)` with no alt text yields
`Block::Paragraph([])`.

`src/channels/format/split.rs:233-235` and `:327-329`:

```rust
    if chunks.is_empty() {
        chunks.push(String::new());
    }
```

`src/channels/discord.rs:213-222` posts every chunk with no emptiness test.
`src/channels/telegram.rs:151` `strip_tool_call_tags`, called at `:2099`, reduces a
tool-call-only reply to `""` before `send_text_chunks`.

### Accepted quirks — do NOT change these

Documented in the module and confirmed deliberate by the audit:

- `ascii_table` measures width with `chars().count()` (CJK/emoji misalign)
- multi-block list items join with `\n\n` (loose CommonMark)
- a single atomic element over the limit is emitted oversized
- a lone escaped `~` round-trips struck
- the documented `#[allow(unused_imports)]` on the `split`/`split_paired` re-export

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Tests | `cargo test --lib channels::format` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/channels/format/ast.rs`, and the five renderers
(`html.rs`, `markdown.rs`, `light.rs`, `plain.rs`, `table.rs` as needed) for the new
inline node.

**Out of scope**: `split.rs`'s "always at least one chunk" contract — the tests pin
it, and the fix belongs at the **call sites**, not here. Per-channel send paths — plan
129 owns them and multiplies the callers; coordinate, do not edit. The accepted quirks
above. `claw-ui`, which deliberately renders raw GFM in the browser and must not be
routed through this module.

## Git workflow

- Branch: `fix/renderer-image-and-empty-chunk`
- Conventional commits, e.g. `fix(format): carry image URLs through the block AST`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Add an image node to the AST

Add `Inline::Image { alt: Vec<Inline>, url: String }` and handle `Tag::Image` /
`TagEnd::Image` exactly as `Link` is handled, reusing the existing `link_urls`
mechanism.

**Verify**: `cargo test --lib channels::format` → all pass (no renderer change yet;
existing tests must not regress).

### Step 2: Render it, per target

Add one arm to each renderer's inline walker. Each target spells it its own way:

- `TelegramHtml` / `MatrixHtml` — an `<a href>` (Telegram cannot render an inline
  image from markdown; a link is the honest representation)
- `StdMarkdown` — `[alt](url)`
- `LightMarkup` — `alt (url)`, matching how it already flattens links
- `Plain` — `alt (url)`

Escaping is per-target and already correct for links; reuse the same path rather than
adding a second one.

**Verify**: `cargo test --lib channels::format` → all pass.

### Step 3: Make the empty-chunk contract safe at the call sites

Do **not** change `split`'s guarantee. Instead, provide a clearly-named way for
callers to skip nothing-to-send: either filter empty chunks in the per-channel send
loop, or add `split_non_empty` alongside the existing function.

Because plan 129 is simultaneously adding `split` call sites to eleven channels, pick
the shape **before** 129 lands and record it in the PR so 129's executor adopts it
rather than inventing a second convention. If 129 has already landed, adapt to what it
did.

**Verify**: `cargo test --lib channels::format` → all pass.

## Test plan

1. `image_url_survives_to_every_render_target` — one assertion per target, that
   `![chart](https://example.test/c.png)` produces the target's expected form and that
   the URL is present.
2. `image_without_alt_text_still_carries_the_url`.
3. `image_inside_a_list_item_and_a_blockquote_renders` — the nesting paths are where
   the `Link` handling is subtlest; mirror the existing link tests.
4. `empty_content_yields_no_sendable_chunk` — whitespace-only input.
5. `tool_call_only_reply_yields_no_sendable_chunk` — the Telegram-shaped case.
6. `existing_split_contract_is_unchanged` — `split` itself still returns one empty
   chunk, so the tests that pin it stay green.

**Mutation check (required).** For test 1, restore the `_ => {}` fallthrough for
`Tag::Image` and confirm it **fails**. Restore afterwards.

**Verify**: `cargo test --lib channels::format` → all pass, including all six.

## Done criteria

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::format` passes, including the six new tests
- [ ] The mutation check was performed and test 1 failed as expected
- [ ] `grep -c 'Tag::Image' src/channels/format/ast.rs` returns a non-zero count
- [ ] None of the five accepted quirks listed in "Current state" has been changed
- [ ] The step-3 convention is recorded in the PR for plan 129 to adopt
- [ ] No files outside `src/channels/format/` are modified (`git status`)
- [ ] `plans/README.md` status row for 131 updated

## STOP conditions

Stop and report back if:

- Adding the inline node requires changing `split`'s atomicity rules. It should not —
  an image is inline, not a block — and if it does, the node shape is wrong.
- An existing `format/` test needs its assertions edited to pass. This module's tests
  are trustworthy and its comments record why each choice was made; a test that must
  change means the change is wrong, not the test.
- You find yourself tempted to fix one of the accepted quirks while in the file. They
  are deliberate. Note anything you disagree with in the PR instead.

## Maintenance notes

- **What interacts with this**: plan 129 adds `format::split` to eleven more channels.
  The step-3 convention must be agreed between the two plans or the fleet ends up with
  two ways of handling nothing-to-send.
- **What a reviewer should scrutinise**: that the image arm reuses the link escaping
  path per target rather than adding a parallel one, and that no accepted quirk moved.
- **Why this module gets a small, tightly-scoped plan**: it was read in full and came
  back nearly clean. Widening this plan would put churn into the best-tested code in
  the subsystem for no finding.
