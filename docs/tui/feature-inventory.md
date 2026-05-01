# RantaiClaw TUI — Feature Inventory

Snapshot of what the inline TUI currently does, **before** comparing
with Hermes / Claude Code. Source of truth: `src/tui/**` on
`feat/inline-tui-hermes-style` (v0.5.3 working tree).

The TUI runs in **inline viewport mode** — the bottom 6 rows of the
terminal are reserved for live UI (stream preview + input box + status
bar). Everything else flows into native terminal scrollback, so users
can scroll up with their normal terminal keys.

---

## 1. Layout (Inline Viewport, 6 rows)

| Row(s) | Pane                  | Description                                                                |
| ------ | --------------------- | -------------------------------------------------------------------------- |
| 1      | Stream preview        | One-line live preview during streaming: spinner + label + last 60 chars of in-progress sentence. Empty when idle. |
| 2–5    | Input box             | Rounded-bordered input pane with `▎ $ you` prefix and placeholder hint.    |
| 6      | Status bar            | `$ provider:model │ tokens/window pct% │ N msgs │ session age` or last error in coral. |

Splash banner + welcome line + `Rantaiclaw v0.5.3 · session XXXXXXXX`
are committed once to scrollback at startup, then the inline viewport
takes over.

---

## 2. Global Key Bindings

| Key                       | Behavior                                                                          |
| ------------------------- | --------------------------------------------------------------------------------- |
| `Enter`                   | Submit input. If autocomplete is open and selection differs from typed text, completes first; second `Enter` submits. |
| `Ctrl+Enter`              | Submit (Kitty-protocol terminals).                                                |
| `Ctrl+J`                  | Insert newline (multi-line prompt).                                               |
| `Shift+Enter`             | Insert newline (terminals that pass it through).                                  |
| `Ctrl+C`                  | Cancel a streaming turn if active; else quit.                                     |
| `Ctrl+D`                  | Quit unconditionally.                                                             |
| `Backspace`               | Delete char left of cursor.                                                       |
| Printable chars           | Append to input buffer; trigger autocomplete refresh on `/`.                      |
| `Up` / `Down`             | Context-dependent — see widgets below.                                            |
| `Esc`                     | Dismiss whichever overlay/picker/dropdown is active.                              |
| `Tab`                     | Complete the highlighted slash-command suggestion (only when autocomplete open).  |
| Mouse                     | Not handled — terminal-native scrollback works for history.                       |

---

## 3. Interactive Widgets

| Widget               | Trigger                                | Navigation                                        | Status |
| -------------------- | -------------------------------------- | ------------------------------------------------- | ------ |
| Slash autocomplete   | Typing `/` (with no spaces yet)        | `Up`/`Down` cycle, `Tab` complete, `Enter` complete-then-submit, `Esc` dismiss. Filters by prefix on every keystroke. | ✅ live |
| Model picker         | `/model` (no args)                     | `Up`/`Down` navigate (wraps), `Enter` select, `Esc` cancel. Pre-selects current `provider:model`. Built from curated lists for enabled providers. | ✅ live (added this session) |
| Help overlay (`/help`)| `/help`                               | **Read-only** — `Esc`/`Tab` accepted by handler but content is now flushed to scrollback as a plain block (no live nav). | ⚠️ degraded — was a tabbed modal pre-inline rewrite, now just dumps to scrollback |
| Streaming spinner    | While `AppState::Streaming`            | None — passive UI element with Braille frames and live snippet. | ✅ live |

---

## 4. Slash Commands (registered)

All commands are dispatched by `src/tui/commands/mod.rs::CommandRegistry`. **None take args via interactive prompts** — args (if any) are typed inline after the command name.

### Session

| Command       | Aliases    | Description                                                | Output        | Args supported |
| ------------- | ---------- | ---------------------------------------------------------- | ------------- | -------------- |
| `/new`        | `/clear`   | Start a new session (fresh ID + history).                  | scrollback msg| none           |
| `/sessions`   |            | List past sessions.                                        | scrollback    | none           |
| `/resume`     |            | Resume a past session by ID prefix.                        | scrollback    | `<id-prefix>`  |
| `/title`      |            | Set a title for the current session.                       | scrollback    | `<text>`       |
| `/search`     |            | Search message history.                                    | scrollback    | `<query>`      |

### Agent control

| Command       | Aliases    | Description                                                | Output        | Args |
| ------------- | ---------- | ---------------------------------------------------------- | ------------- | ---- |
| `/retry`      |            | Re-run the last user message.                              | resubmits     | none |
| `/undo`       |            | Remove last assistant + user exchange.                     | scrollback    | none |
| `/stop`       |            | Stop ongoing agent generation.                             | -             | none |

### Configuration

| Command       | Aliases    | Description                                                | Output        | Args |
| ------------- | ---------- | ---------------------------------------------------------- | ------------- | ---- |
| `/status`     |            | Show current session and agent status.                     | scrollback    | none |
| `/debug`      |            | Toggle debug mode.                                         | scrollback    | none |
| `/config`     |            | Inspect or set configuration values.                       | scrollback    | `[key] [value]` |
| `/doctor`     |            | Run diagnostics and health checks.                         | scrollback    | none |
| `/platforms`  |            | Show active communication platforms.                       | scrollback    | none |

### Memory & context

| Command       | Aliases    | Description                                                | Output        | Args |
| ------------- | ---------- | ---------------------------------------------------------- | ------------- | ---- |
| `/memory`     |            | Add, list, or remove memory entries.                       | scrollback    | subcommand |
| `/forget`     |            | Remove a specific memory entry by key.                     | scrollback    | `<key>` |
| `/compress`   |            | Compress current context by summarizing older messages.    | scrollback    | none |

### Skills & personality

| Command       | Aliases    | Description                                                | Output        | Args |
| ------------- | ---------- | ---------------------------------------------------------- | ------------- | ---- |
| `/skills`     |            | List available skills.                                     | scrollback    | none |
| `/skill`      |            | Invoke or inspect a skill by name.                         | scrollback    | `<name> [args]` |
| `/personality`|            | Show or switch the agent personality.                      | scrollback    | `[name]` |
| `/insights`   |            | Show session and message statistics.                       | scrollback    | none |

### Model / cost

| Command       | Aliases    | Description                                                | Output                         | Args |
| ------------- | ---------- | ---------------------------------------------------------- | ------------------------------ | ---- |
| `/model`      |            | Pick or change the active model.                           | **interactive picker** (no args) / scrollback msg (with args) | `[provider:model]` |
| `/usage`      |            | Show token usage statistics.                               | scrollback                     | none |

### Other

| Command       | Aliases    | Description                                                | Output                         | Args |
| ------------- | ---------- | ---------------------------------------------------------- | ------------------------------ | ---- |
| `/cron`       |            | Manage scheduled tasks.                                    | scrollback                     | subcommand |
| `/help`       |            | Show available commands.                                   | scrollback (was modal overlay) | none |
| `/quit`       | `/exit`    | Exit the application.                                      | -                              | none |

**Total:** 24 user-visible commands. None besides `/model` use arrow-key navigation; everything else is text-args or read-only output.

---

## 5. Streaming Behavior

| Aspect                  | Behavior |
| ----------------------- | -------- |
| Provider streaming      | Real SSE for OpenAI-compatible providers (covers MiniMax, Moonshot, GLM, Qwen, Venice, etc.); native streaming for Anthropic/OpenAI/Gemini via their own provider impls. |
| Pacing                  | Coarse server chunks (e.g. MiniMax mega-chunks) are re-split into whitespace-bounded words and emitted with a 20 ms gap so the live preview feels token-by-token. |
| `<think>` blocks        | Filtered out of the visible stream — reasoning tags never reach the user. |
| Markdown rendering      | Inline (bold/italic/code) + block-level (`#`/`##`/`###` headings, `-`/`*` bullets) applied to both committed scrollback lines and the streaming preview. |
| Newline behavior        | Each `\n` in the stream commits the completed line to scrollback via `Terminal::insert_before`; the in-progress tail stays in the preview pane. |
| TUI tick rate           | 16 ms (~60 fps) during streaming, 100 ms idle. |
| Cancellation            | `Ctrl+C` cancels mid-stream; partial text is preserved with `[cancelled]` marker. |

---

## 6. Profiles & Configuration (TUI surface)

| Aspect                    | Behavior |
| ------------------------- | -------- |
| Profile selection         | `--profile <name>` CLI flag or `RANTAICLAW_PROFILE` env. The TUI reads the active profile's `config.toml` at launch. |
| Available providers list  | `default_provider` + any unique providers in `model_routes`, exposed via `ctx.available_providers` and consumed by the model picker. |
| Status-bar model display  | Reflects `default_provider:default_model` from config (overridden by `--model` flag if given, or by `/model` selection at runtime). |
| Tracing                   | TUI tick logs go to `~/.rantaiclaw/logs/tui-YYYY-MM-DD.log` (avoids alt-screen corruption). |

---

## 7. What's NOT Interactive Yet (gap list)

| Surface                              | Current behavior                                                | Could become interactive |
| ------------------------------------ | --------------------------------------------------------------- | ------------------------ |
| `/help`                              | Dumps full command list to scrollback (one block).              | Up/Down list of commands with inline descriptions; `Enter` to fill `/cmd ` into input buffer. |
| `/skills` / `/skill`                 | Lists skills; invocation requires typing the name.              | Up/Down skill picker; `Enter` to invoke (or pre-fill args). |
| `/sessions` / `/resume`              | Lists IDs; resume by typing prefix.                             | Up/Down session list with title/last-message preview; `Enter` to resume. |
| `/personality`                       | Switch by typing name.                                          | Up/Down preset picker (executive-assistant, friendly-companion, …). |
| `/cron`                              | Subcommand-style scrollback output.                             | Up/Down cron-job picker with edit/delete/run-now actions. |
| `/memory`                            | List/get/clear via subcommands.                                 | Up/Down memory entry list with preview pane. |
| `/config`                            | `key = value` text edits.                                       | Up/Down list of editable keys; `Enter` to enter edit mode. |
| `/doctor`                            | Static report.                                                  | Up/Down list of findings; `Enter` to expand a finding's hint. |
| `/insights`                          | Static stats block.                                             | Could swap to a small dashboard pane. |
| Status bar (`/sb`/`/statusbar`)     | Toggle on/off only.                                             | Could pick between brief/full layouts. |

So today **only `/model`** uses arrow-key navigation. Everything else
either takes inline args or just emits a scrollback block.

---

## 8. Quick Reference — How to Drive It

1. `./target/release-fast/rantaiclaw` — launches with the active profile.
2. Type a prompt → `Enter` to send; `Ctrl+J` for newline.
3. `/` brings up autocomplete; arrows navigate suggestions.
4. `/model` opens the model picker; arrows + `Enter` to switch.
5. `Esc` always dismisses the topmost overlay/picker/dropdown.
6. `Ctrl+C` cancels streaming; second `Ctrl+C` (or while idle) quits.
7. Terminal-native scrollback (mouse wheel / `PgUp`) reads history.

---

*Generated 2026-04-30 from working tree on `feat/inline-tui-hermes-style`.*
