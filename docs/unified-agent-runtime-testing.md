# Testing the unified-agent-runtime branch (isolated sandbox)

`dev/sandbox.sh` runs the locally-built `rantaiclaw` against a **persistent,
isolated state dir** (`dev-sandbox/`, gitignored). It redirects `HOME`,
`XDG_DATA_HOME`, `XDG_CONFIG_HOME`, and `RANTAICLAW_PROFILE` so nothing touches
your real `~/.rantaiclaw`, `~/.local/share`, or `~/.config`. Verified: the real
`~/.rantaiclaw` fingerprint is unchanged after running, and all state lands under
`dev-sandbox/home/.rantaiclaw/...`.

```bash
dev/sandbox.sh --version          # smoke (no config / no key needed)
dev/sandbox.sh doctor             # environment checks
rm -rf dev-sandbox                # reset to a clean sandbox
```

For real LLM turns, point it at a key (kept inside the sandbox config):

```bash
RANTAICLAW_SANDBOX_API_KEY=sk-... dev/sandbox.sh agent -m "what is 2+2?"
# or edit dev-sandbox/home/.rantaiclaw/profiles/sandbox/config.toml after first run
```

## Automated tests (the real validation)

The behavior is covered by the in-tree suites — run them scoped (never the whole
crate at once; it OOMs):

```bash
cargo test --lib agent::          # 174 — loop, prompt, memory scoping, gate test
cargo test --lib channels::       # 596 — channel prompt + memory + owner gate
cargo test --lib gateway::        # 62  — SSE, channel approval owner gate
cargo test --lib approval         # 72  — owner gate + ApprovalBackend
cargo test --lib memory           # recall_layered, scoping
```

Key gate test for the loop collapse:
`agent::tests::xml_dispatcher_multi_turn_preserves_structured_tool_history`.

## Manual checks per change

| Change | How to exercise in the sandbox |
|---|---|
| **PR1.1 unified prompt** | `dev/sandbox.sh agent -m "hi"` (needs key) — one builder now feeds CLI/channels/gateway; the TUI path is the same builder. Compare against `dev/sandbox.sh` (TUI). |
| **PR2 one loop** | `dev/sandbox.sh agent -m "run a tool then summarize"` — both the TUI and CLI now drive `run_structured_loop`; a tool-calling turn that completes confirms it. |
| **PR3 owner-gate** | Set `approval_owners` in the sandbox config (or via a channel). With it empty, an approval-required tool is auto-denied on a channel; with your sender id listed, only *your* `Y`/`A` reply is honored. (Full path needs a configured channel + key.) |
| **PR3b strict parity** | `/autonomy strict` (TUI) then confirm `shell` is absent from the tool list on both TUI and a channel. |
| **PR4 memory scoping** | `dev/sandbox.sh memory list` — store lives at `dev-sandbox/.../workspace/memory/brain.db`. Conversation-scoping (`recall_layered`) is unit-tested; the channel/Agent paths key memory by `ConversationKey` (`channel:sender[:thread]`). |
| **`XDG_DATA_HOME` test-isolation fix** | `cargo test --lib gateway::` repeated — `sse_chat_emits_chunk_then_done` is now stable (was flaky). |

## Notes

- First run seeds `dev-sandbox/home/.rantaiclaw/profiles/sandbox/config.toml`
  (placeholder key). It is **never** clobbered on later runs — edit it freely.
- The sandbox config pre-includes a `[channels_config] approval_owners = []`
  block as the PR3 test knob.
- `cargo build` runs with your real `HOME` (so the `~/.cargo` cache is reused);
  only the *running* binary sees the sandbox `HOME`.
