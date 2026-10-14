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
| Model picker         | `/model` (no args)                     | `Up`/`Down` navigate (wraps), typing filters the visible rows, `Enter` select, `Esc` cancel. Pre-selects current `provider:model`. Lists each enabled provider's whole catalog (cached models, else the curated list) plus the active model when the catalog lacks it. A `custom:<base-url>` provider lists that endpoint's cached models, and `custom:<url>:<model>` is a valid target. The empty text is true: a configured provider with no known models says so and points at `/model <provider:model>`; with no provider at all it points at `rantaiclaw setup provider`. | ✅ live (added this session) |
| Help overlay (`/help`)| `/help`                               | **Read-only** — `Esc`/`Tab` accepted by handler but content is now flushed to scrollback as a plain block (no live nav). | ⚠️ degraded — was a tabbed modal pre-inline rewrite, now just dumps to scrollback |
| Streaming spinner    | While `AppState::Streaming`            | None — passive UI element with Braille frames and live snippet. | ✅ live |

---

## 4. Slash Commands (registered)

All commands are dispatched by `src/tui/commands/mod.rs::CommandRegistry`. **None take args via interactive prompts** — args (if any) are typed inline after the command name.

### Session

| Command       | Aliases    | Description                                                | Output        | Args supported |
| ------------- | ---------- | ---------------------------------------------------------- | ------------- | -------------- |
| `/new`        | `/clear`   | Start a new session; the next message carries the system prompt only. | scrollback msg| none           |
| `/sessions`   |            | List past sessions; picking one resumes it and seeds its turns.        | scrollback    | none           |
| `/resume`     |            | Resume a past session by ID prefix.                        | scrollback    | `<id-prefix>`  |
| `/title`      |            | Set a title for the current session.                       | scrollback    | `<text>`       |
| `/search`     |            | Search message history.                                    | scrollback    | `<query>`      |

### Agent control

| Command       | Aliases    | Description                                                | Output        | Args |
| ------------- | ---------- | ---------------------------------------------------------- | ------------- | ---- |
| `/retry`      |            | Re-run the last user message; the old reply is removed from the screen, the stored session and the model's context. | resubmits     | none |
| `/undo`       |            | Remove the last assistant + user exchange from the screen, the stored session and the model's context. | scrollback    | none |
| `/stop`       |            | Stop ongoing agent generation.                             | -             | none |

### Configuration

| Command       | Aliases    | Description                                                | Output        | Args |
| ------------- | ---------- | ---------------------------------------------------------- | ------------- | ---- |
| `/status`     |            | Show current session and agent status.                     | scrollback    | none |
| `/config`     |            | Inspect configuration values. It sets nothing: the model is changed with `/model`, everything else with `/setup`. | scrollback    | `[key]` |
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
| `/model`      |            | Pick or change the active model. Builds the provider first, then saves `default_provider`/`default_model` and reloads the running agent. | **interactive picker** (no args) / scrollback msg (with args) | `[provider:model]` |
| `/usage`      |            | Show token usage for this session: prompt, completion and total tokens summed over every turn. `not reported` until a provider reports usage. Resets on `/new` and on resuming another session. | scrollback                     | none |

`/model <provider:model>` is refused before anything is written. The TUI builds the provider through the
same factory the agent uses, so a target that cannot run (`/model nosuch:xyz`) answers `Model not
changed: <provider> cannot run this model: <reason>`: nothing is saved, no reload is sent, and the
status bar keeps naming the model the agent runs.

The status bar moves only when the reload reports the new agent in place. Until then the agent is still
on the old model, so a `/model` switch that is still loading does not change the label. If the rebuild
fails, the chat says `Model not changed: the agent could not be rebuilt: <reason>` and the status bar
keeps the old model.

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
| Status-bar model display  | Reflects `default_provider:default_model` from config (overridden by `--model` flag if given, or by `/model` selection at runtime). A `/model` selection moves it only once the reload reports the new agent in place, so the label always names the model the agent is actually running. |
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
| `/config`                            | Shows current values in an inspector; sets nothing.            | Could gain a filter over the config tree. |
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
