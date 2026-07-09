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
cargo test --lib agent::          # 178 — loop, prompt, memory scoping, gate test, injected-backend
cargo test --lib channels::       # 603 — channel prompt + memory + owner gate + tool relay
cargo test --lib gateway::        # 68  — SSE, channel approval owner gate, web-modal backend + endpoint
cargo test --lib approval         # 86  — owner gate + ApprovalBackend
cargo test --lib memory           # recall_layered, scoping
```

Key gate tests:
- loop collapse: `agent::loop_::tests::xml_dispatcher_multi_turn_preserves_structured_tool_history`
- safety text per surface: `agent::prompt::tests::safety_section_channel_*`
- in-chat approval seam: `agent::loop_::tests::injected_backend_overrides_non_cli_auto_deny`
  (None backend on a channel auto-denies; an injected backend runs the tool)
- relay owner gate: `channels::approval_relay::tests::tool_reply_*` +
  `chat_relay_backend_*` (post→approve→Yes, timeout→deny)

## Manual checks per change

| Change | How to exercise in the sandbox |
|---|---|
| **PR1.1 unified prompt** | `dev/sandbox.sh agent -m "hi"` (needs key) — one builder now feeds CLI/channels/gateway; the TUI path is the same builder. Compare against `dev/sandbox.sh` (TUI). |
| **PR2 one loop** | `dev/sandbox.sh agent -m "run a tool then summarize"` — both the TUI and CLI now drive `run_structured_loop`; a tool-calling turn that completes confirms it. |
| **PR3 owner-gate** | Set `approval_owners` in the sandbox config (or via a channel). With it empty, an approval-required tool is auto-denied on a channel; with your sender id listed, only *your* `Y`/`A` reply is honored. (Full path needs a configured channel + key.) |
| **PR3-relay in-chat approval** | With `approval_owners = ["<your-id>"]` and tool-gating on (default), ask the channel bot to run a non-read-only tool. It posts `🔧 …/approve <tool>` to the chat and waits; reply `/approve <tool>` (as the owner) → it runs, `/deny <tool>` → it fails. A non-owner's `/approve` is refused; no reply within 5 min auto-denies. Empty `approval_owners` ⇒ no prompt, straight auto-deny. |
| **PR3-webmodal in-browser approval** | Hit the console SSE chat (`POST /api/v1/agent/chat` with `Accept: text/event-stream`) and have the agent call a non-read-only tool. The stream emits `{"type":"approval_request","id":…,"tool":…}`; resolve with `POST /api/v1/approvals/{id}` body `{"approve":true}` → the tool runs and the stream resumes; `{"approve":false}` → it's denied. No resolve within 5 min auto-denies. `autonomous_tools = true` skips gating entirely. |
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
