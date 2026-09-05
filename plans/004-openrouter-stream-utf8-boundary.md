# Plan 004: Fix OpenRouter streaming abort when a UTF-8 codepoint straddles a chunk boundary

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan in
> `plans/README.md`.
>
> **Drift check (run first)**: `git diff --stat 4d35107..HEAD -- src/providers/openrouter.rs`
> If the file changed since this plan was written, compare the "Current state"
> excerpt against the live code; on a mismatch, treat it as a STOP condition.

## Status

- **Priority**: P1
- **Effort**: S
- **Risk**: LOW
- **Depends on**: none
- **Category**: bug
- **Planned at**: commit `4d35107`, 2026-07-18

## Why this matters

OpenRouter is a first-class streaming provider. Its `chat_stream` decodes each
network chunk with `std::str::from_utf8(&bytes)?`, which returns `Err` — ending
the stream — whenever a multi-byte UTF-8 character (emoji, CJK, accented Latin,
em-dash) is split across two TCP/SSE chunks. The result: responses containing
non-ASCII text intermittently truncate mid-answer with "OpenRouter streamed
non-UTF8 bytes", and the partial reply is what the user sees. Frequency scales
with non-ASCII density and network fragmentation — reliably reproducible for
CJK/emoji-heavy content. The sibling compatible-provider path already handles
this correctly.

## Current state

- `src/providers/openrouter.rs` — `chat_stream` (starts ~line 416). The buggy
  decode (lines 462-466):
  ```rust
  while let Some(chunk) = byte_stream.next().await {
      let bytes = chunk?;
      let text = std::str::from_utf8(&bytes)
          .map_err(|e| anyhow::anyhow!("OpenRouter streamed non-UTF8 bytes: {e}"))?;
      sse_buffer.push_str(text);

      // Process complete lines.
      while let Some(pos) = sse_buffer.find('\n') {
          let line: String = sse_buffer.drain(..=pos).collect();
          ...
  ```
  `sse_buffer` is a `String` accumulated across chunks (declared ~line 459), and
  line-splitting on `\n` is already correct. Only the per-chunk UTF-8 decode is
  wrong: it must not error on an incomplete trailing multi-byte sequence — it
  must carry those bytes forward to the next chunk.

- **The correct pattern to mirror** — `src/providers/compatible.rs:1767`
  (verified) uses `String::from_utf8_lossy` on the accumulating buffer. Read the
  surrounding function (`grep -n "from_utf8_lossy" src/providers/compatible.rs`)
  to see the exact accumulation shape.

## Commands you will need

| Purpose | Command | Expected on success |
|---------|---------|---------------------|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Provider tests | `cargo test -p rantaiclaw providers::openrouter` (or `cargo test openrouter`) | all pass, incl. new |

## Scope

**In scope**:
- `src/providers/openrouter.rs` — the chunk-decode loop only.
- New unit test in the same file's `#[cfg(test)]` module (or add one).

**Out of scope** (do NOT touch):
- `src/providers/compatible.rs` — it is the reference; leave it.
- The SSE line-parsing (`parse_sse_line`), tool-call accumulation, or request
  building — only the byte→str decode changes.
- Other providers.

## Git workflow

- Branch: `advisor/004-openrouter-stream-utf8-boundary`
- One commit; message e.g.
  `fix(providers): don't abort OpenRouter stream on UTF-8 char split across chunks`.
- Do NOT push or open a PR unless instructed.

## Steps

### Step 1: Buffer raw bytes and decode only complete UTF-8

Change the decode so incomplete trailing bytes are carried to the next chunk.
Two acceptable approaches — pick the one that reads cleanest against the existing
code:

**Approach A (byte buffer + incremental valid-prefix decode):** keep a
`Vec<u8>` pending buffer. On each chunk, append bytes, then decode the longest
valid UTF-8 prefix via `std::str::from_utf8`; on `Err(e)`, use
`e.valid_up_to()` to split — push the valid prefix into `sse_buffer`, retain the
remaining (incomplete) tail bytes in the `Vec<u8>` for next time. On stream end,
if tail bytes remain, decode them lossily.

**Approach B (mirror compatible.rs):** accumulate raw bytes and use
`String::from_utf8_lossy` exactly as `compatible.rs:1767` does. Only choose this
if the compatible path also defers decoding until it has enough bytes; a naive
`from_utf8_lossy` per chunk would insert replacement characters (U+FFFD) at every
split boundary, which is *better than aborting* but still corrupts multi-byte
chars. **Prefer Approach A** — it is lossless across boundaries. Read
compatible.rs before deciding; match the codebase's actual chosen tradeoff.

Target shape for Approach A:
```rust
let mut pending: Vec<u8> = Vec::new();
while let Some(chunk) = byte_stream.next().await {
    let bytes = chunk?;
    pending.extend_from_slice(&bytes);
    let valid_up_to = match std::str::from_utf8(&pending) {
        Ok(_) => pending.len(),
        Err(e) => e.valid_up_to(),
    };
    // SAFETY-free: valid_up_to bytes are known-valid UTF-8.
    let good = String::from_utf8_lossy(&pending[..valid_up_to]).into_owned();
    sse_buffer.push_str(&good);
    pending.drain(..valid_up_to);   // keep the incomplete tail
    // ... existing line-splitting loop on sse_buffer unchanged ...
}
```
Leave the rest of the loop (line splitting, `parse_sse_line`, delta handling)
exactly as-is.

**Verify**: `cargo build 2>&1 | tail -5` → compiles.

### Step 2: Ensure no `non-UTF8 bytes` error path remains

**Verify**: `grep -n "non-UTF8" src/providers/openrouter.rs` → no matches (the
error string is gone).

## Test plan

- New unit test in `src/providers/openrouter.rs` `#[cfg(test)]` module. It does
  NOT need a network: extract or exercise the byte-accumulation logic. If the
  decode loop is inline in `chat_stream` and hard to isolate, refactor the
  byte-accumulation into a small testable helper, e.g.
  `fn push_decoded(pending: &mut Vec<u8>, chunk: &[u8], out: &mut String)`, and
  call it from `chat_stream`. Test cases:
  1. `utf8_split_across_chunks_not_lost`: feed a 4-byte emoji (e.g. "🦀" =
     `[0xF0, 0x9F, 0xA6, 0x80]`) split as `[0xF0, 0x9F]` then `[0xA6, 0x80]`;
     assert the accumulated output equals "🦀" with no replacement char.
  2. `cjk_split_across_chunks`: split a 3-byte CJK codepoint across two chunks;
     assert it reassembles.
  3. `ascii_unaffected`: plain ASCII in one chunk decodes unchanged.
  4. `trailing_incomplete_at_stream_end`: if the stream ends with an incomplete
     sequence (truncated response), the helper must not panic (decode remaining
     lossily).
- Model the test module after any existing `#[cfg(test)]` in the providers
  directory: `grep -rln "#\[cfg(test)\]" src/providers/`.
- Verification: `cargo test openrouter` → all pass including the new tests.

## Done criteria

Machine-checkable. ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `grep -n "non-UTF8" src/providers/openrouter.rs` returns no matches
- [ ] `grep -n "from_utf8(&bytes)" src/providers/openrouter.rs` returns no matches (the strict per-chunk decode is gone)
- [ ] `cargo test openrouter` passes; the emoji-split and CJK-split tests exist and pass
- [ ] Only `src/providers/openrouter.rs` modified (`git status`)
- [ ] `plans/README.md` status row updated

## STOP conditions

Stop and report back (do not improvise) if:

- The `chat_stream` decode loop does not match the excerpt (drift since `4d35107`).
- `compatible.rs`'s approach turns out to also be lossy per-chunk (U+FFFD at
  boundaries) — then report it as a second, related bug rather than copying it;
  Approach A is still correct for OpenRouter.

## Maintenance notes

- Any new streaming provider must use the same carry-the-incomplete-tail
  decoding — a per-chunk `from_utf8` is the recurring trap. Consider whether the
  extracted helper belongs in a shared `providers` util so future providers
  reuse it (do not force it in this plan; note it for follow-up).
- Reviewer should confirm the incomplete-tail bytes are never dropped on the
  happy path and never cause an infinite loop when a chunk contains only a
  partial codepoint.
