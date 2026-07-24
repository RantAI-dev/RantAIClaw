# Changelog

All notable changes to RantaiClaw are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.10.0-alpha] — 2026-07-24

Full skills lifecycle across every surface. `install`/`enable`/`disable`/`update`/
`remove` now behave consistently in the CLI, the TUI, and the gateway API; remote
skills carry an explicit trust boundary; and several install-path hardenings land.
Minor bump: new CLI subcommands, new gateway skill-management routes, and a config
schema bump (15 → 16).

### Added

- **`skills enable` / `skills disable` CLI commands** — flip a skill on or off
  without editing config by hand; skill entries now match case-insensitively.
- **Gateway skills management API** — `install`, `enable`, `disable`, and
  `uninstall` routes (`POST`/`PUT`/`DELETE /api/v1/skills*`) plus read-side status
  fields, so the web console's skills panel drives real actions instead of a
  read-only list.
- **TUI skills lifecycle parity** — enable/disable toggle with live reload,
  unified name matching, and a gated-row UX for skills blocked by policy.
- **Remote-skill trust boundary** — the open-skills source is pinned to an
  explicit ref, remote entries are de-duplicated and shown compactly, and provider
  `api_key` is stored at rest (config schema **15 → 16**).

### Fixed

- **`skills remove` is a true uninstall** — removes across all roots (bundled,
  ClawHub, and `install`ed), and no longer 404s when the target skill was
  previously disabled.
- **`skills update` is non-destructive** — atomic swap that never deletes the
  existing copy before the fetch succeeds, so a failed update can't leave a skill
  missing.

### Security

- **Install-path hardening** — `install-deps` downloads are anchored to the target
  directory and sha256-verified; ClawHub installs enforce the security scan and
  require sha256 + an HTTPS base URL.
- **Skill write-tools gated** — authoring/write tools require the matching autonomy
  level, are rate-limited, and are owner-only; `author_skill` frontmatter list
  values are sanitized to prevent injection.

### Notes

- Pins claw-ui **v0.3.8** (unchanged) for `ui install`.

## [0.9.1-alpha] — 2026-07-23

Tool-approval UX is made honest and consistent across the TUI and the web
console, and the web console can now approve shell commands instead of hanging.
Patch bump: behavior/label fixes plus a backward-compatible `always` field on the
approvals API.

### Fixed

- **TUI approval** — the `[Y]` chip now reads "yes (session)" (Y allowlists the
  basename for the whole session, not "once"); `/deny` cancels the entire turn
  like the inline `N`/`Esc` key; and concurrent blocked commands no longer strand
  an approval off-screen (the box advances to the next still-queued request).
  Corrected a stale "auto-deny on timeout" comment and dropped inert cascade code.
- **Web console** — a Supervised `shell` command not on the allowlist no longer
  hangs the turn: its command-level approval is surfaced through the same
  in-browser modal + `POST /api/v1/approvals/{id}`, mirroring the TUI (one modal
  per command; cascading `a && b` prompts per blocker).

### Added

- **Web-console approval parity with the TUI** — the tool-approval modal gains an
  **Always** option (`POST /api/v1/approvals/{id}` accepts `{ approve, always }`;
  back-compatible — `{ approve }` still works), a Deny that cancels the turn, and
  an "Always" grant that persists across the conversation's messages.

### Notes

- Requires claw-ui **v0.3.8** for the console "Always" button (the pinned default
  `ui install` fetches).

## [0.9.0-alpha] — 2026-07-22

Scheduled tasks ("cron") become a first-class, cross-surface feature: create and
manage recurring or one-shot jobs — shell **or** agent — from the HTTP API, the
CLI, the TUI, the web console, and conversationally from a chat channel, with the
agent's scheduled output delivered back to the chat it was asked in. Minor bump:
new runtime-contract surfaces (API routes, CLI flags, TUI command) plus one agent
tool removed.

### Added

- **Gateway** — `/api/v1/cron*` HTTP API: list / create / update / delete a job,
  force-run (`POST /cron/{id}/run`), and read run history (`GET /cron/{id}/runs`).
  Auth-gated like the rest of `/api/v1`.
- **CLI** — `rantaiclaw cron add|add-at|add-every|once` gain `--agent` (create an
  agent job; the positional is the prompt) and `--model`; new `cron run <id>`
  (force-run + record) and `cron runs <id> [--limit]`.
- **TUI** — `/cron` opens an interactive jobs picker; a job's detail panel exposes
  `[r]` run · `[p]` pause/resume · `[d]` delete; `/doctor` reports scheduler health.
- **Web console (claw-ui)** — a Schedules panel: create shell/agent jobs, edit,
  pause/resume, run-now, and view run history (requires the paired claw-ui release).
- **Channels** — conversational scheduling: when asked on an announce-capable
  channel (Telegram/Discord/Slack/Mattermost) to send a recurring message, the
  agent creates a `cron_add` agent job whose output is delivered back to that chat.

### Changed

- Cron engine fixes: one-shot shell jobs no longer re-fire; in-flight guard against
  overlapping runs; agent-job timeout; `[scheduler].enabled` gate honored.

### Removed

- The redundant `schedule` agent tool (shell-only, no delivery) is removed in favor
  of the delivery-capable `cron_add` (+ `cron_list/remove/update/run/runs`). Both
  wrote the same store; keeping the weaker one caused models to schedule jobs that
  never delivered. Migration: use `cron_add`.

### Fixed

- Deterministic channel delivery: a `cron_add` from an announce-capable channel now
  defaults `delivery` to the origin chat even when the model omits it, so scheduled
  messages reliably arrive instead of landing only in run history.

## [0.8.3-alpha] — 2026-07-20

The per-platform reply rendering that v0.8.2-alpha introduced for Telegram is now
wired to the rest of the channels. Every channel except Matrix renders the
agent's GitHub-Flavored Markdown into the platform's own dialect instead of
leaking `**bold**`/`##`/tables as literal text. This changes outbound behavior on
twelve channels; it is versioned as a patch.

### Changed

- **Discord, DingTalk, Mattermost** render replies as `StdMarkdown` — CommonMark
  markup is kept; tables become an aligned ASCII grid in a fenced block on
  Discord/DingTalk (which have no native tables) and stay native pipe tables on
  Mattermost. Discord's naive char-count splitter (which could cut a code fence
  in half) is replaced by the fence-aware splitter.
- **Slack and WhatsApp (Cloud + Web)** render replies as `LightMarkup` — the
  single-char markup these platforms actually use: `**bold**` → `*bold*`, links
  → Slack's `<url|text>` or WhatsApp's `text (url)`, tables → an ASCII grid.
  Slack output escapes `&`/`<`/`>` as its `text` field requires.
- **Signal, QQ, Linq, IRC, iMessage, Nextcloud Talk, Lark, Email, CLI** strip
  markup to readable `Plain` text (headings uppercased, emphasis removed, links
  → `text (url)`, tables → aligned ASCII). IRC keeps its own 512-byte PRIVMSG
  line splitter, now fed the rendered text.
- See `docs/reference/channels.md` → "Reply Formatting" for the full matrix.

### Notes

- **Matrix is intentionally not wired** — it already renders GFM natively via
  `matrix-sdk`, so nothing leaks, and the `channel-matrix` feature currently
  cannot be built (`matrix-sdk 0.16` overflows the type-check recursion budget,
  which is why CI omits `--all-features`). A render target is deferred until the
  feature builds again.
- DingTalk's `\*literal\*` escaping is not verified against a live DingTalk
  client; if its markdown parser shows the backslash, that escaping will be
  dropped in a follow-up.

## [0.8.2-alpha] — 2026-07-19

Telegram replies now render as HTML instead of shipping raw markdown, so
headings, rules, and tables stop leaking as literal text. This does change the
Telegram channel's outbound behavior; it is versioned as a patch.

### Changed

- **Telegram renders replies as HTML.** `#`/`##`/`###` headings become bold,
  `---` rules become a line, runs of blank lines collapse, and code blocks and
  tables render as `<pre>` — instead of leaking `#`/`---`/`|` through Telegram's
  legacy `parse_mode=Markdown`. Each chunk carries a plain-text twin that is
  sent as a fallback if Telegram rejects the HTML, and streaming draft edits are
  rendered as plain text mid-response.
- The agent's markdown is now rendered per platform through a shared `format`
  library, selected via a new `render_target()` method on the `Channel` trait.
  Only Telegram is wired so far; every other channel keeps its current behavior
  (the plain-text baseline default). Matrix wiring is written but held back until
  `matrix-sdk` builds under `--all-features` again.

### Notes

- Not yet verified against a live Telegram bot at release time — the rendering
  is covered by unit tests and CI, but the actual send has not been exercised
  end-to-end. Verify on a real chat when installing this build.

## [0.8.1-alpha] — 2026-07-19

A focused fix to the interactive TUI chat composer so paste and the input box
stay in sync, matching Claude-Code-style behavior. Patch bump — no runtime
contract (config / CLI / provider / channel) changes.

### Added

- **Dynamic composer height.** The inline chat input now grows to fit its
  content (1–6 text rows) and shrinks back, instead of a fixed 2-row box.

### Fixed

- **Cursor no longer drifts on wrapped lines.** Wrapping and caret placement now
  derive from one shared layout pass, so the terminal cursor always sits on the
  real character.
- **Paste works under tmux.** Multi-line pastes are reconstructed from raw key
  events when the terminal does not deliver bracketed paste, and collapse to a
  `[Pasted text #N +M lines]` placeholder instead of splitting into many turns.
- **No ghost composer box.** Resizing the inline viewport now clears the box at
  its real tracked position, fixing duplicate top-borders that accumulated on
  every grow/shrink.
- A lone `Tab` in the composer no longer inserts a literal tab character.

## [0.8.0-alpha] — 2026-07-18

A large accumulated batch spanning multiple review/hardening sprints since
0.7.15-alpha. The headline is a broad security sweep of the exposure surface
(two RCE-class allowlist bypasses, two SSRF vectors, three secret-leak paths),
alongside profile isolation, per-channel markdown rendering, and a provider-auth
parity fix for the rig-core migration. Minor bump (not patch) because several
changes alter runtime contracts — see **Changed** for the behavior flags.

### Security

- **Command-allowlist bypass hardening.** Closed two allowlist-completeness
  gaps that allowed arbitrary command execution past the shell high-risk gate:
  `find -execdir/-exec` and `git --upload-pack=`/short-flag smuggling both
  reached a shell without gate review. The gate now inspects the effective verb
  after global flags and rejects the exec-delegating forms.
- **SSRF defense on outbound fetch tools.** `http_request` and `browser` now
  pin the resolved address and block DNS-rebinding / redirect-to-internal
  (link-local, loopback, RFC1918) so a hostile URL cannot pivot to gateway or
  cloud-metadata endpoints.
- **Secret-leak fixes.** Config redaction now covers the IRC / Lark / WhatsApp
  credential stems it previously missed; channel config no longer echoes secret
  values in diagnostics; MCP subprocesses are spawned with a cleared
  environment (`env_clear` + explicit allowlist) so provider keys don't leak to
  third-party MCP servers.
- **Secret-file permissions.** The master key / secret store is written `0600`
  without a world-readable window (TOCTOU-safe create).
- **Webhook signature enforcement is fail-closed.** Unsigned / bad-signature
  inbound webhook requests are rejected rather than processed.
- **Inbound image handling is workspace-confined** and guarded against
  decompression-bomb payloads.

### Added

- **Per-profile data isolation.** `sessions.db` and `kb.db` (knowledge base)
  now live under the active profile directory instead of a shared global path,
  with a WAL-checkpoint-first migration on load. WhatsApp defaults are also
  per-profile.
- **Channel markdown rendering.** A shared formatting library renders agent
  output per channel (light-markup, standard-markdown, and HTML variants for
  platforms that need it) instead of leaking raw markdown.
- **Knowledge-base OCR ingestion** for image documents.
- **Gateway credential-awareness.** Switching to a provider with no usable
  credential now surfaces a warning instead of silently failing later.
- **TUI paste handling** collapses large pastes and shreds them from scrollback.

### Changed

- **Anthropic/Gemini provider auth parity restored (rig-core migration).** The
  default provider path routes special auth modes correctly again: Gemini
  `GEMINI_API_KEY`/`GOOGLE_API_KEY` env keys resolve, Gemini CLI OAuth and
  Anthropic setup-tokens (`sk-ant-oat01-`) route to the legacy providers (which
  the rig client cannot serve), and Anthropic prompt caching is re-enabled on
  the rig path. Consequence: **`anthropic-custom` now requires an API key**, and
  the legacy provider modules are permanent (their planned removal is
  cancelled). Setup-token / CLI-OAuth requests do not stream (documented
  tradeoff); the API-key path is unchanged (streaming + native tools).
- **`GET /api/v1/providers` now requires a bearer token.** It was the only
  `/api/v1` data route without an auth check; it now honors the same pairing
  gate as the rest of the API.
- **Shell commands now inherit common tooling env vars.** The shell tool's env
  allowlist (`SAFE_ENV_VARS`) was so narrow that `kubectl`, `docker` (remote /
  rootless), `aws`/`gcloud`, `git`-over-ssh-agent, and proxied commands failed
  with "not found" / "no credentials" even in the TUI. It now also forwards the
  HTTP(S) proxy vars, `KUBECONFIG`, `DOCKER_HOST`, `SSH_AUTH_SOCK`,
  `GIT_SSH_COMMAND`, `AWS_PROFILE`/`AWS_REGION`/`AWS_DEFAULT_REGION`,
  `GOOGLE_APPLICATION_CREDENTIALS`, `XDG_RUNTIME_DIR`, and `KRB5CCNAME`. These
  are functional pointers (paths / sockets / selectors), never secret values —
  API keys and tokens are still stripped (local-capability widening, CLAUDE.md §3.6).
- **README no longer teaches `allow_public_bind = true`** in its example; the
  exposure boundary stays localhost-by-default.
- **Bundled claw-ui console pinned to v0.3.4** (the `ui install` default). Picks
  up the render-time `</think>` / `[IMAGE:…]` strip fixes and the
  provider-switch no-credential toast that pairs with the gateway
  credential-warning above.

### Fixed

- **Anthropic `max_tokens` regression.** The default (rig) Anthropic path sent
  `max_tokens: None`, which the API rejects for every non-claude-4 model
  ("max_tokens must be set for Anthropic") — so all claude-3.x requests errored
  before being sent. Fixed with a per-model default.
- **Streaming UTF-8 chunk-boundary splits** in the OpenRouter / OpenAI-compatible
  providers no longer abort a stream when a multibyte character lands on a
  buffer boundary.
- **MCP supervisor backoff** no longer resets its retry counter on every respawn
  (runaway restart loop) and honors the configured backoff.
- **Service install (systemd --user)** now captures `PATH` /
  `WorkingDirectory` at install time, so tools like `kubectl` resolve when the
  agent runs as a service.
- **Failing-provider hot-reload** no longer drops the configured autonomy level.
- **Docker tool timeout** escalates to `SIGKILL` after `SIGTERM` instead of
  hanging.
- **Flaky-test env-lock fragmentation.** Consolidated ~17 per-module env mutexes
  into one crate-shared lock so channel-owner / pairing / persona / config tests
  stop clobbering each other under parallel `cargo test`.
- Numerous smaller correctness fixes across channels, gateway config
  lost-update, think-tag stream handling, and the agent tool-loop
  force-summary path.

### Performance

- HTTP client reuse across runtime-proxy calls, lazy-compiled regexes, a
  batched knowledge-base ingest transaction, and a lighter rerank helper.

## [0.7.15-alpha] — 2026-07-13

### Fixed

- **Stopping a web-chat turn now actually stops the work.** Cancelling a
  long-running prompt (e.g. an install) — or hitting the shell timeout — used to
  leave the underlying subprocesses running, so the agent kept "thinking" about
  the cancelled prompt and the next turn stalled. The native runtime now runs
  each shell command as its own process group and reaps the **whole group**
  (SIGTERM → grace → SIGKILL) on cancel/timeout; docker containers are stopped
  via signal forwarding; and a pending tool approval is aborted together with
  the turn (#172, #174, #177).
- **Chat session persistence is atomic and concurrency-safe.** Turns are written
  in a single `IMMEDIATE` transaction with a busy-timeout, and a cancelled,
  errored, or empty turn is no longer half-persisted (#173).
- **pty / ssh session hygiene.** Unique default pty session names (no cross-turn
  collision that destroyed another turn's session), no orphaned session on a
  cancelled `start`, and ssh exec now closes its channel on timeout and evicts
  dead sessions from the registry (#175, #176).
- **Shell timeout raised 60s → 10min**, with clearer tool-result reporting
  (stderr folded in on success, exit code surfaced on failure) and a bounded
  read to cap output memory (#174).

### Changed

- **Console bumped to claw-ui v0.3.2.** Hardens web-chat cancel/cleanup (no UI
  wedge, no context bleed, no stuck streaming), adds a mobile off-canvas sidebar
  and a focus-trapped tool-approval modal, and lands a batch of accessibility +
  rendering fixes (icon aria-labels, skip link, keyboard-operable session rows,
  `<think>`/`[object Object]` render guards). `ui install` now pins v0.3.2 by
  default.

## [0.7.14-alpha] — 2026-07-12

### Fixed

- **Web-console login over a LAN IP no longer gets stuck.** The console now
  ships claw-ui **v0.3.1**, which marks the `rc_session` cookie `Secure` based
  on the request's real protocol instead of unconditionally (the prebuilt
  release always runs `NODE_ENV=production`). Browsers no longer drop the
  session cookie over plain `http://` at a LAN IP, so login persists and reaches
  the chat page. `ui install` pins v0.3.1 by default.
- **`rantaiclaw uninstall --all` / `--purge` no longer orphans the web console.**
  Uninstall stopped the daemon but not the `ui start` console, which is tracked
  separately in `~/.rantaiclaw/ui/.run` — so the full-tree wipe removed that file
  out from under a still-running process, leaving it holding the port and
  untrackable by `ui stop`. Uninstall now stops the console before the wipe.

## [0.7.13-alpha] — 2026-07-12

### Added

- **Configurable web-console bind address (`[ui] host` / `ui start --host`).**
  The console binds `127.0.0.1` (loopback) by default; set `[ui] host = "0.0.0.0"`
  (or a specific IP), or pass `ui start --host <addr>`, to reach it from other
  devices on your LAN. `ui start` output adapts: a loopback bind prints the
  `ssh -L` port-forward hint; a network bind prints the reachable LAN URL plus a
  login-state note (🔒 when a console login is set, ⚠ otherwise — enable one with
  `rantaiclaw setup login`). The console is a full agent-control surface, so the
  default stays loopback and LAN exposure is an explicit operator opt-in.

### Fixed

- **`rantaiclaw ui install` no longer orphans a running console.** It now stops
  the running console (via its `.run` PID file) before wiping the install dir, so
  reinstalling (e.g. after `update`) no longer leaves an untrackable process
  holding the port.

## [0.7.12-alpha] — 2026-07-12

### Added

- **`rantaiclaw ui start` prints an SSH port-forward hint on remote hosts.** The
  web console binds `127.0.0.1` only, so on a VM accessed over SSH it isn't
  reachable from your local machine. When a remote session is detected (via
  `SSH_CONNECTION`), `ui start` now prints a ready-to-copy
  `ssh -L <port>:127.0.0.1:<port> <user>@<server-ip>` command plus the URL — on
  both the fresh-launch and already-running paths. The bind stays loopback-only;
  no exposure change.

## [0.7.11-alpha] — 2026-07-12

### Changed

- **The web console now installs from a prebuilt, signed release instead of
  building from source.** `rantaiclaw ui install` downloads a SHA256- and
  cosign-signed claw-ui standalone artifact, verifies it (failing closed on a
  missing signature), and extracts it; `rantaiclaw ui start` serves the
  production build with `node server.js` bound to `127.0.0.1`. This removes the
  on-machine `git clone` + JS build (and its `npm audit`/`sharp` noise), gives
  the console the same SHA256 + cosign supply-chain guarantee as the binary
  updater, and makes installs fast. Notes: `--ref` now selects a **release
  tag** (was a git ref), and **`node`** is now a prerequisite for `ui start`.

## [0.7.10-alpha] — 2026-07-11

### Fixed

- **The TUI no longer leaves empty "Untitled" sessions on launch.** `TuiContext`
  opened a `"tui"` session in the store before any input, so opening and closing
  the TUI (or `/new`) with no message left empty, untitled 0-message sessions
  accumulating in `session list` and the web console's session sidebar (a shared
  `sessions.db`). Session creation is now deferred until the first message is
  persisted; resume (`--resume` / `/resume`) still binds the existing session
  immediately, and titling/multi-turn are unchanged.

## [0.7.9-alpha] — 2026-07-11

### Fixed

- **Channel owners are recognized under any of their identity forms.** The owner
  gate matched only the single sender the runtime resolved (username-preferred
  for Telegram), while the per-channel chat allowlist checks every form — so an
  owner added by numeric id was silently treated as a guest whenever that sender
  also had a username. Owner matching now checks every form via `can_approve_any`
  and `ChannelMessage.sender_aliases` (`/claim` already stored both). No
  permission is widened: matching stays case-sensitive with `@` stripped, and
  `*`/empty-list semantics are unchanged.
- **Channel agents no longer self-refuse owner-only tools.** When an approval
  owner chats, the channel system prompt now states the sender is a verified
  owner, so cautious models stop declining owner-only tools (`manage_permissions`,
  `issue_pairing_code`) without calling them. The runtime gate remains the sole
  enforcer; non-owner turns get no owner context.
- **A leaked temp-dir workspace marker no longer shadows the active profile.** An
  `active_workspace.toml` pointing under the OS temp dir (e.g. left by a
  non-hermetic test) is ignored with a warning, preventing a config split-brain
  where owner/config edits appeared not to apply until the marker was removed.

## [0.7.8-alpha] — 2026-07-11

### Added

- **Knowledge Graph API now exposes an intelligence capability signal.** Both
  `GET /api/v1/kb/graph` and `GET /api/v1/kb/documents/{id}/intelligence` return
  a `capability { intelligence_enabled, extraction_model }` block so the console
  can distinguish "extraction disabled" from "no entities yet" instead of an
  indistinguishable empty graph. The model name is not a secret; additive — no
  existing field changed.
- **Scope-aware graph statistics.** `/graph` stats gain `corpus_entities`,
  `corpus_relations`, and a `truncated` flag (scope-wide, honouring `?group=`)
  alongside the existing `total_nodes`/`total_edges`, so the console can render an
  honest "showing N of M" rather than presenting a capped count as the total.
  Graph edges now carry a `weight` (merged duplicate relation rows), surfaced on
  the API and in the `kb graph` CLI output (JSON + TOON).
- **Hard node ceiling for graph queries.** `GET /api/v1/kb/graph?limit=` is now
  clamped by a server-side `GRAPH_HARD_CAP` (5000), independent of `limit` and
  `KB_GRAPH_MAX_NODES`, bounding the returned node set.

### Fixed

- **Graph edges are de-duplicated by `(source, target, relation_type)`** and node
  `degree` is recomputed from the deduped set, so repeated per-document
  extractions no longer inflate edges or degree.
- **`GET /api/v1/kb/documents/{id}/intelligence` returns `404` for a missing
  document** instead of `200` with empty arrays (which was indistinguishable from
  a document that exists but has no entities), matching `GET /documents/{id}`.

## [0.7.7-alpha] — 2026-07-10

### Added

- **`rantaiclaw ui start` now self-heals a stale or foreign process on the
  gateway port** instead of blindly reusing whatever is listening. It probes the
  gateway's public `GET /api/v1/version` (which now also reports an opaque
  `config_fingerprint` of the loaded config) and: reuses a current gateway;
  restarts one running a stale binary version or stale config (e.g. an
  out-of-date login `password_hash` after a failed hot-reload); and errors —
  never signalling the process — when the port is held by a foreign or
  unidentified app. Both stop paths are identity-guarded against PID reuse.

### Changed

- **Default gateway port is now `9393`** (was `3000`, which collides with other
  local dev servers such as Next.js/Vite). Configs written by rantaiclaw are
  unchanged — they serialize `port` explicitly and keep their value; only fresh
  configs (or hand-written configs omitting `[gateway] port`) pick up the new
  default. Bumps the config schema to **v12** (no migration action needed).
  Docker/compose images keep an explicit `3000` (container-isolated) and are
  unaffected.

### Fixed

- **The TUI console login gate now renders full-screen** instead of being clipped
  inside the inline chat viewport (the password field and hint were cut off and
  the welcome splash bled through behind it). It takes over the terminal via the
  alt-screen path like the first-run wizard, with a centred, padded card; the
  banner + chat return after a successful unlock.

## [0.7.6-alpha] — 2026-07-09

### Added

- **Optional single-operator console/TUI login (username + password).** A new
  `[gateway.login]` config section (`username`, argon2 `password_hash`) gates the
  web console (claw-ui) and the TUI when a password is set — enable or disable it
  with `rantaiclaw setup login`. `POST /login` verifies the credential and issues
  the same bearer token that already guards `/api/v1/*`; the public
  `GET /api/v1/auth/info` reports only `{ "login_required": bool }` (never the
  username). Verification is stateless — the gateway keeps no login session;
  repeated failed attempts are rate-limited and locked out. When login is enabled,
  `rantaiclaw ui start` no longer auto-injects a token, so the browser prompts for
  the password. Default-off: with no `password_hash`, behavior is unchanged.
  - Requires a claw-ui build that ships the login page (posts to `/login`).
  - Adds the `argon2` dependency (one-way password hashing).
  - Bumps the config schema to **v11** (additive; no migration action needed).

### Fixed

- **Console-login build/test regressions caught post-merge.** Restored the
  `#[cfg(feature = "tui")]` gate on the `SetupOverlayState` re-export (the new
  login-gate import had displaced it, breaking `--no-default-features` /
  hardware-only / browser-native builds), and updated the setup-orchestration
  tests for the new `login` section (canonical order, section count, and the
  valid `setup <topic>` list). Also satisfied the strict clippy delta gate.

## [0.7.5-alpha] — 2026-07-08

### Added

- **`GET /readyz` readiness endpoint.** Returns 200 when every supervised
  component is healthy and 503 (with the offending component names) when any
  is in the `error` state, so a Kubernetes/compose readiness probe can pull a
  crash-looping instance out of rotation. `/health` is unchanged (liveness).

### Fixed

- **Agent-chat API metrics now reach `/metrics`.** `POST /api/v1/agent/chat`
  (sync and streaming) built its agent with a throwaway per-request observer,
  so its metrics were never scraped. It now uses the gateway's shared observer.

### Internal

- Added deterministic tests for the self-update integrity gates
  (`compute_sha256`, `verify_sha256`, `verify_installed_binary`).

## [0.7.4-alpha] — 2026-07-08

### Changed

- **The daemon now shuts down gracefully on `SIGTERM`, not just Ctrl+C.**
  `systemctl stop` / `launchctl stop` / a plain `kill` send `SIGTERM`, which
  the daemon previously took as an immediate terminate — skipping its cleanup
  and leaking auto-managed containers (e.g. SearXNG) plus a stale daemon
  sentinel that confused `profile use`/handoff. It now runs the full graceful
  path on either signal, and the generated systemd unit sets
  `KillSignal=SIGTERM` + `KillMode=mixed` + `TimeoutStopSec=30` to bound the
  stop.
- **The gateway drains in-flight HTTP requests on shutdown.** On stop, the
  gateway stops accepting new connections and lets in-flight requests (webhook
  processing, API calls, streaming chats) finish — bounded to 8s — instead of
  the connection being severed mid-request.

### Security

- Bumped `calamine` 0.30 → 0.36 (quick-xml → 0.41), removing the vulnerable
  parser (RUSTSEC-2026-0194/0195, DoS-class) from the `kb-office` xlsx path,
  and `crossbeam-epoch` → 0.9.20 (RUSTSEC-2026-0204).

## [0.7.3-alpha] — 2026-07-07

### Fixed

- **The web console self-heals a stale gateway token.** `ui start` reused the
  token remembered in `.env.local` without checking it still works; a token
  issued by a previous gateway instance (an update or restart that reset
  `paired_tokens`) was rejected with 401 on every request, so the console
  showed *"Gateway Offline"* until `.env.local` was cleared by hand. `ui start`
  now probes an authed endpoint with the stored token and, only on an explicit
  401/403, drops it and re-pairs via the on-demand pairing path — fresh
  installs, updates, gateway restarts, and switching between `rantaiclaw ui
  start` and wrapper launchers (e.g. Copilot's `copilot-web`) all recover with
  no manual steps. Transient probe failures keep the token (fail-safe), and a
  valid token is never re-paired.

## [0.7.2-alpha] — 2026-07-07

### Security

- **`GET /api/v1/config` no longer leaks per-provider API keys.** The endpoint
  cleared every other at-rest secret but missed `config.provider_api_keys` (a
  per-provider key map, decrypted in memory), so its "redacted" response
  returned every provider key in plaintext to any authenticated client — and
  into the web console's browser response. `provider_api_keys` is now redacted
  like the rest. Key *presence* is still available via `GET /api/v1/secrets`.

### Fixed

- **The web console reflects a provider (or any config) changed in the TUI
  without a daemon restart.** The gateway served `GET /api/v1/config` from the
  config it loaded at startup and never watched `config.toml`, so a TUI edit
  didn't reach the console until a restart. The gateway now hot-reloads its
  running config when `config.toml` changes (same decrypt pass as startup). The
  `config.toml` watcher moved from `tui/` to `config/` so both surfaces share it.
- **The web console now pairs against an already-running gateway.** `rantaiclaw
  ui start` only auto-paired when it spawned the gateway itself (reading the
  one-time code from its own log); against a running daemon it skipped pairing,
  wrote an empty token, and the console got 401 *"Gateway requires pairing…"* —
  the common fresh-install case. It now mints a short-lived on-demand `gateway`
  pairing code and exchanges it via `POST /pair` (which the gateway already
  honours from the store), so it pairs whether or not it started the gateway,
  and can re-pair after a lost `.env.local` without a restart. `require_pairing`
  stays authoritative.

## [0.7.1-alpha] — 2026-07-06

### Fixed

- **`uninstall` stops a running daemon before wiping its data.** A daemon bound
  to a profile rewrote its dir every few seconds, so `uninstall` looked like it
  did nothing — the profile reappeared immediately. It now reads each target
  profile's `.daemon_active` sentinel and signals live foreground daemons
  (SIGTERM→SIGKILL, with the PID confirmed to be a rantaiclaw daemon via
  `/proc/<pid>/cmdline` before any signal) before removing data; service-managed
  units are still torn down via `service uninstall`.
- **`uninstall --purge` removes a bootstrap-copied `~/.cargo/bin` binary.** A
  binary the installer *copied* into `~/.cargo/bin` was misclassified as a cargo
  install, so `--purge` refused it and advised `cargo uninstall rantaiclaw` —
  which fails with "did not match any packages". `classify` now confirms cargo
  actually records the binary (via `.crates2.json`/`.crates.toml`) before
  deferring; untracked binaries are removed directly. Genuine cargo installs
  still defer.
- **Shell-rc cleanup only touches the installer's own PATH block.** It matched
  any line containing "rantaiclaw" — missing the installer's real amendment
  (whose PATH export has no "rantaiclaw" in it) while clobbering the user's own
  aliases/config. It now comments out only the single PATH line directly beneath
  the `# Added by RantaiClaw installer` marker, and is idempotent.
- **`uninstall` now reports the still-installed binary + how to remove it**, so a
  data-only uninstall no longer reads as a no-op (the binary self-recreates a
  fresh `~/.rantaiclaw` on next launch).
- Fixed a test-only daemon-teardown fork bomb: under `cargo test`,
  `current_exe service uninstall` re-ran every test matching "uninstall", each
  spawning again. Guarded with `cfg!(test)`; production behaviour is unchanged.

## [0.7.0-alpha] — 2026-07-04

### Added

- **Configure the Knowledge Base from setup, onboard, and the gateway.** KB API
  keys (embedding + optional OCR/vision) now live in a `[knowledge]` config
  section, encrypted at rest like `api_key`. Set them via the `rantaiclaw setup
  knowledge` wizard section, the interactive first-run wizard's Integrations
  step (so bare `rantaiclaw setup` / `onboard` offers KB), or the gateway
  `GET`/`PUT /api/v1/config/knowledge` endpoints (presence-only responses, keys
  never returned). Env `KB_EMBEDDING_API_KEY` / `KB_EXTRACT_VISION_API_KEY`
  still override config at load; `OPENROUTER_API_KEY` remains the final
  fallback. Previously KB keys were environment-variable-only. Schema bumped to
  v10 (additive). A missing key now surfaces a clear `kb_not_configured` message
  instead of a raw failure.

### Fixed

- `reload_config` now decrypts the `[knowledge]` keys (like `Config::load_or_init`),
  so a wizard/`setup knowledge` run leaves the running agent with usable KB
  credentials instead of a raw encrypted blob until restart.

## [0.6.99-alpha] — 2026-07-03

### Fixed

- **Setup banner now shows the RantaiClaw wordmark.** The `rantaiclaw onboard`
  quick-setup and interactive wizard rendered a stale ASCII wordmark; the banner
  now displays RANTAICLAW, sourced from the shared onboarding banner asset
  (borders and tagline unchanged).
- **Firmware sketch headers rebranded to RantaiClaw.** The Arduino Uno and Uno-Q
  bridge sketch header comments now read RantaiClaw (comments only; no functional
  change).

## [0.6.98-alpha] — 2026-07-01

### Added

- **Manage a Telegram channel from the web console.** `POST /api/v1/channels/telegram`
  now fully manages Telegram rather than only connecting experimentally:
  `bot_token` is optional, so you can update `allowed_users` without re-entering
  it; connect / update / disconnect trigger a managed-daemon reload so the change
  applies without a manual restart; and the bot token is now **encrypted at rest**
  in `config.toml`, like `api_key`. The console's Channels panel gains a full
  Telegram card (connect, editable allowlist, disconnect) and lists the remaining
  channels as "under development". (#121)

## [0.6.97-alpha] — 2026-06-30

### Fixed

- **Re-extraction now refreshes entity confidence; hard delete cleans up the
  graph.** `upsert_entity` used `ON CONFLICT(canonical_key) DO NOTHING`, so a
  re-extract (e.g. after the confidence-prompt fix) could never lift a stale
  value — cross-document entities created by an older binary stayed at 0%
  because they are never garbage-collected. It now does
  `DO UPDATE SET confidence = max(confidence, excluded.confidence)`, keeping
  first-seen identity but lifting confidence to the highest across mentions.
  Separately, a **hard** `delete_document` now clears the document's
  `entity_mention` / `entity_relation` rows and GCs orphaned entities in the
  same transaction (previously it left them behind); **soft** delete still
  preserves intelligence since the document is recoverable. Re-extract after
  upgrading to refresh existing confidences. (#119)

## [0.6.96-alpha] — 2026-06-30

### Fixed

- **`rantaiclaw ui install` no longer aborts on a dirty managed checkout.** The
  console install prefers `bun`, and `bun install` rewrites the tracked
  `bun.lock` on every run, leaving `~/.rantaiclaw/ui` dirty; a bare
  `git pull --ff-only` then failed — and a user's global `pull.rebase = true`
  turned `--ff-only` into a rebase that aborts with "you have unstaged changes".
  The update path now discards local churn (`git checkout -- .`) and pins
  `pull.rebase=false` before the fast-forward, since the checkout is
  tool-managed (not for hand edits). Still `--ff-only`, so genuinely diverged
  history errors loudly. (#117)

## [0.6.95-alpha] — 2026-06-30

### Added

- **KB GraphRAG — graph-augmented retrieval (off by default).** The SP-2
  knowledge graph now improves answers, not just visualisations. When
  `KB_GRAPHRAG_ENABLED=true`, retrieval matches query terms to graph entities
  (case-insensitive name match, ≥3 chars — no LLM), expands one hop along
  relations (capped by `KB_GRAPHRAG_MAX_NEIGHBORS`, default `20`), and feeds the
  chunks that mention those entities into the existing **RRF fusion** as a third
  ranked list alongside the vector and BM25 arms. Graph candidates never replace
  the other arms, and a chunk already found by vector/BM25 keeps its score and
  metadata. The handle is attached at both retrieval build sites — the CLI
  `kb search` path (which the agent shells out to) and the `POST /api/v1/kb/search`
  HTTP endpoint — so enabling the flag improves chat answers with no other change.
  New `IntelligenceStore::graph_expand_chunks`. Fail-soft: a graph error degrades
  to plain vector+BM25 retrieval. When disabled, retrieval is bit-for-bit
  unchanged. Env-only config, no schema drift. (#115)

### Fixed

- **Document Intelligence confidence no longer collapses to 0.** The extractor
  prompt's structural example used `"confidence":0.0`, which the model echoed
  back verbatim — every entity/relation surfaced as 0% in the graph UI. The
  prompt now uses realistic non-zero examples and an explicit "never 0"
  instruction, and parsed confidences are sanitised (non-positive/NaN → `0.5`,
  clamped to `(0, 1]`) so a single misbehaving response can never resurface as
  0%. Re-extract existing documents after upgrading to refill their confidence.
  (#114)

## [0.6.94-alpha] — 2026-06-30

### Added

- **KB cross-document Document Intelligence (off by default).** A new
  `src/kb/intelligence/` subsystem extracts entities + relations from ingested
  documents (one LLM call per chunk, fail-soft, plus a pure-Rust regex pass for
  emails/URLs) and resolves the *same* entity across documents into a single
  global node by canonical key — a real cross-document knowledge graph, beyond
  the per-document scoping of the TS parent. Entities, mentions and relations are
  stored in three new sqlite tables (additive `kb.db` migration, schema_version
  1→2) behind a focused `IntelligenceStore` trait. New endpoints
  `GET /api/v1/kb/documents/{id}/intelligence`, `GET /api/v1/kb/graph`,
  `POST /api/v1/kb/documents/{id}/re-extract`, and CLI `kb intelligence` /
  `kb graph` (TOON). When `KB_INTELLIGENCE_ENABLED=true`, extraction runs as a
  fire-and-forget task at ingest, so ingest latency and reliability are
  unaffected. Configured via env only — `KB_INTELLIGENCE_ENABLED` (default
  `false`), `KB_INTELLIGENCE_MODEL` (default `openai/gpt-4.1-nano`),
  `KB_INTELLIGENCE_RESOLUTION` (default `exact`), `KB_GRAPH_MAX_NODES` (default
  `200`) — so the config schema fingerprint is unchanged. Graph-aware retrieval
  (GraphRAG) and embedding-based fuzzy resolution are deferred. The web console
  ships a Knowledge Graph explorer + per-document intelligence drawer in the
  separate `claw-ui` repo.

## [0.6.93-alpha] — 2026-06-29

### Fixed

- **WhatsApp owner is recognized over LID.** The inbound WhatsApp sender was the
  opaque LID (e.g. `+207550217756908`), never the real phone number, so it never
  matched `approval_owners` — the operator was silently treated as a guest and
  every owner-only feature (cron, permissions, owner commands) was unavailable,
  surfacing as empty/"no final answer" replies. The sender is now resolved
  LID→phone-number via wa-rs's `lid_pn_cache` (the same mapping the reply-target
  fix uses), so owner and allowlist matching run on the real number. Unmapped
  LIDs keep the prior unverifiable-allowlist behavior. You can now also set your
  real number in `allowed_numbers` instead of `*`.

## [0.6.92-alpha] — 2026-06-29

### Added

- **Channel config applies immediately.** Finishing WhatsApp setup now
  (re)starts the daemon automatically, and `rantaiclaw service start` restarts an
  already-running service instead of a silent no-op — so a channel/config change
  takes effect without a manual restart. New `service::apply_channel_config`
  (restart if installed, else install + start; quiet so it is safe in the TUI
  setup overlay).

### Fixed

- **One process per channel (single-runner).** A TUI now defers channel startup
  to a live daemon (via the `.daemon_active` sentinel), and every channel
  listener holds an advisory per-channel lock. Prevents two processes (daemon +
  TUI) from running the same channel — the cause of duplicate/contradictory
  WhatsApp replies, `Telegram getUpdates 409 Conflict` flapping, and gateway
  `:3000` "address already in use" spam.
- **Replies are answer-only.** The user-facing message never contains the
  internal `[Used tools: …]` annotation (kept in history) and is never empty —
  a graceful fallback is sent when the model ends a turn after tool calls with
  no final text.

## [0.6.91-alpha] — 2026-06-29

### Fixed

- **WhatsApp Web now answers LID-addressed chats.** WhatsApp delivers many
  direct chats via a privacy LID (`<id>@lid`, not a phone number); replies were
  sent to the bare LID JID, which lands in a hidden thread the recipient never
  sees — the bot appeared to "type but never reply". Replies and the typing
  indicator now resolve the LID to the phone-number thread via wa-rs's learned
  LID↔PN mapping, falling back to the original JID for groups and unmapped LIDs.

## [0.6.90-alpha] — 2026-06-26

### Fixed

- **KB design/scan-heavy PDFs now extract via OCR.** A port regression
  (`UnpdfExtractor` returned `pages: None`) collapsed the per-page sufficiency
  heuristic, so image-layout PDFs (brochures, scans) were accepted with thin
  text and never routed to OCR — they ingested as semantically-thin documents
  the agent could not read. Restored real page counts and added a
  text/file-size density guard so large low-text PDFs route to the vision-LLM
  extractor. Vision OCR now also falls back to the embedding API key when its
  own key is unset.
- **KB retrieval surfaces more documents per query.** A single answer could
  cluster in a few documents; retrieval now fetches a wider candidate pool and
  caps chunks per document so multi-document questions span more sources.
- **Ingest no longer leaves orphan 0-chunk documents** — the document row is
  rolled back if chunk storage fails.

### Added

- **Reliable KB enumeration.** The retrieval context now prepends the full
  document inventory, so "what's in this knowledge base?" lists every document
  regardless of which chunks were retrieved.
- **Ingest observability.** Extraction quality (chars / pages / low-text
  density) is logged and returned in the ingest response so poor extractions
  are visible instead of silent.

### Security

- Bumped `lopdf` 0.34/0.38 → 0.42.0 ([RUSTSEC-2026-0187], PDF-parser
  stack-overflow DoS on crafted input — the KB parses untrusted uploaded PDFs)
  and `quinn-proto` → 0.11.15 ([RUSTSEC-2026-0185], remote memory exhaustion).

[RUSTSEC-2026-0187]: https://rustsec.org/advisories/RUSTSEC-2026-0187
[RUSTSEC-2026-0185]: https://rustsec.org/advisories/RUSTSEC-2026-0185

## [0.6.89-alpha] — 2026-06-23

### Changed

- **Easy-mode defaults — RantaiClaw is now useful out of the box (config schema v9).**
  Fresh installs ship with local capability tools **enabled** instead of
  deny-by-default, so the agent can search the web and fetch URLs without an
  operator editing config first:
  - `[web_search] enabled = true`
  - `[browser] enabled = true`
  - `[http_request] enabled = true`, `allowed_domains = ["*"]` (new allow-all
    wildcard), `max_response_size = 5 MiB`, `timeout_secs = 20`
  - `[autonomy] block_high_risk_commands = false` (e.g. `curl`/`wget` run; they
    still pass the normal allowlist/approval path)

  **Unchanged (secure at the exposure boundary):** autonomy `level` stays
  `Supervised`; gateway pairing, localhost bind, `allow_public_bind = false`,
  and rate limits are untouched — exposing the agent to the network is still
  opt-in. **Existing configs are not modified**: the v8→v9 migration preserves
  any values you set explicitly; only omitted keys pick up the new defaults.
  The engineering protocol (`CLAUDE.md` §3.6) was rewritten to match: usable by
  default for local capability, deny-by-default only at the exposure boundary.

## [0.6.88-alpha] — 2026-06-23

### Fixed

- **Channel conversations now survive a daemon restart.** Per-sender
  conversation history (user + assistant turns, keyed `channel_sender`) was
  held only in memory and rebuilt empty on every boot, so restarting the
  channels daemon wiped every live thread (e.g. Telegram "forgot" the
  conversation). History is now persisted write-through to a `channel_history`
  table in the existing `brain.db` and reloaded at startup, so threads resume
  exactly where they left off across restarts. Enabled automatically when the
  memory backend is `sqlite`; markdown/none backends keep the prior in-memory
  behavior. Persistence failures degrade gracefully (log + in-memory only) and
  never block message handling; stored history is bounded by the existing
  history cap. A dedicated `ChannelHistoryStore` opens its own WAL connection
  with `busy_timeout` so it coexists with the memory backend's connection.

## [0.6.87-alpha] — 2026-06-23

### Fixed

- **Autonomy level now hot-reloads on running channels — no daemon restart.**
  Switching the approval preset (`rantaiclaw autonomy off`/`smart`/`strict`/…)
  previously only took effect for a freshly-started `channels run`/daemon: the
  per-message config reload synced the command *allowlist* but never the
  autonomy *level*, so e.g. `autonomy off` left the live Telegram daemon still
  enforcing the old `Supervised` gate until restart. The level is now shared
  via an interior `Arc<RwLock>` (mirroring `runtime_allowlist`) and re-applied
  on each config-file change, so all channel surfaces pick it up at the next
  message. Reads go through `SecurityPolicy::effective_autonomy()`; the channel
  reload calls `set_autonomy()`. (Command allowlist, approval owners, and the
  guest gate already hot-reloaded; `forbidden_paths` and the medium/high-risk
  approval flags still require a restart by design — they narrow the security
  boundary and are applied at boot only.)

## [0.6.86-alpha] — 2026-06-23

### Added

- **Native MiniMax embedding provider (`embo-01`) for semantic memory.** Set
  `[memory] embedding_provider = "minimax"` (with `embedding_model = "embo-01"`,
  `embedding_dimensions = 1536`) to enable vector recall using MiniMax. The
  embedding API key reuses the main MiniMax provider key — no second credential
  to enter. Uses MiniMax's native request format (`texts` + `type: "db"`) and
  response envelope (`vectors` + `base_resp`), so non-zero `base_resp.status_code`
  surfaces as an explicit error. Defaults to the global base
  `https://api.minimax.io/v1`; override with `MINIMAX_EMBED_BASE_URL` (e.g. the
  CN base `https://api.minimaxi.com/v1`). A `GroupId` is optional and only sent
  when `MINIMAX_GROUP_ID` is set. Verified live against the global endpoint.

## [0.6.85-alpha] — 2026-06-23

### Fixed

- **Telegram permission setup no longer silently fails.** `approval_owners`
  matching now strips a leading `@` the same way the `allowed_users` gate does,
  so a hand-written `approval_owners = ["@dramnerf"]` actually authorizes sender
  `dramnerf` (it previously did a raw compare and silently denied, leaving the
  operator unable to approve anything — the bot looked dead). Matching stays
  case-sensitive, identical to `allowed_users`, so the two gates never disagree.
- **Telegram config error messages named the wrong section.** "Missing
  `[channels.telegram]`" / "check `[channels.telegram]`" now correctly say
  `[channels_config.telegram]` — following the old text created an ignored
  section and left the bot unconfigured.
- **Config file is now written `0600`.** `Config::save()` restricts the file to
  the owner before publishing it (it carries bot tokens / API keys); previously
  the daemon only *warned* that the on-disk config was world-readable.
- **Corrected inaccurate `[autonomy]`/`[agent]` documentation.** The
  `autonomy.level` doc said `read_only` (mistyping it errors with "unknown
  variant"); it now says `readonly`. Stale `Default:` annotations were fixed
  (`max_tool_iterations` 10→25, `max_actions_per_hour` 100→200), and
  `max_cost_per_day_cents` is now documented as tracked-for-reporting-only — it
  is not enforced as a hard stop in the agent loop.

### Added

- **`rantaiclaw channel unbind-telegram <identity>`.** Removes a username,
  numeric id, or the `*` wildcard from the Telegram allowlist — so you can lock
  an open (`["*"]`) allowlist down to explicit entries without hand-editing
  `config.toml`. Warns when the removal empties the allowlist.
- **Actionable remediation in blocked / rate-limited / path-policy errors.**
  When a tool is blocked, rate-limited, denied on a channel, or hits a
  path-policy wall, the error now names the concrete knob to fix it
  (`rantaiclaw autonomy full` / `[autonomy].allowed_commands` /
  `[autonomy].max_actions_per_hour` / `[channels_config].approval_owners` /
  `autonomous_tools` / the workspace + `forbidden_paths` policy) instead of
  dead-ending. Applied across the shell, file, pdf, cron, glob, schedule, and
  task tools.

## [0.6.84-alpha] — 2026-06-19

### Fixed

- **In-chat approvals no longer get stuck.** Approving a gated tool/command over
  a channel now accepts forgiving replies — a bare `/approve`, `approve`, `yes`,
  `y`, `ok` (or `/deny`, `no`) resolves the single pending request, and naming
  the command resolves the one pending tool — instead of requiring the exact
  `/approve <tool>` / `/allow <basename>` token, which silently hung to the
  5-minute auto-deny. With multiple requests pending, the bot lists them and asks
  you to pick one. Owner-authority is unchanged (only owners approve; deny is
  honored from anyone).

### Added

- **Live config reload for the channel runtime — no restart.** `rantaiclaw
  channels run` now hot-reloads owners (`approval_owners`), the guest capability
  ceiling (`guest_allowed_tools`/`guest_allowed_commands`), and the owner command
  allowlist (`autonomy.allowed_commands`) when `config.toml` changes — CLI / TUI
  `/permissions` / chat edits apply on the next message (~instantly), reusing the
  existing per-message config-stamp reload. (Removing a command from the allowlist
  still takes effect on restart — the live sync only widens.)
- **Manage the owner command allowlist from the permissions surface.** A new
  `allow-command` target on `rantaiclaw permissions`, the `/permissions` TUI
  command, and the owner-only `manage_permissions` chat tool edits
  `autonomy.allowed_commands` — the command BASENAMES an owner may run without an
  approval prompt (e.g. `permissions add allow-command kubectl`, or ask the bot
  "let me run kubectl"). Basenames here, not globs — globs are the guest
  `command` list.

## [0.6.83-alpha] — 2026-06-19

### Changed

- **Raised default limits that were cutting chats off mid-answer.** These are
  the values applied to configs that don't set them explicitly (existing configs
  with explicit values are unaffected):
  - `[autonomy] max_actions_per_hour`: `20` → `200` (hourly tool-call budget —
    the old default was exhausted quickly, surfacing as "Rate limit exceeded"
    mid-turn).
  - `[agent] max_tool_iterations`: `25` → `50` (per-turn tool-loop cap — long
    multi-tool tasks no longer stop early with a "reached maximum tool calls"
    message).
  - `[channels_config] message_timeout_secs`: `300` → `600` (per-turn channel
    budget; still scales up to 4x with tool-loop depth).
  - `[reliability] provider_retries`: `2` → `3` (a transient provider/network
    blip is less likely to fail the whole turn).

  Security-by-default is unchanged: approval gating (`autonomous_tools=false`,
  empty `approval_owners`), `require_approval_for_medium_risk`, and the shell
  allowlist keep their conservative defaults.

## [0.6.82-alpha] — 2026-06-18

Universal, on-demand pairing codes: mint a fresh code without restarting the
daemon, and self-onboard via `/bind` / `/claim` on every multi-user channel.

### Added

- **On-demand pairing codes (no daemon restart).** `rantaiclaw channels pair
  [--channel <name>] [--ttl <min>] [--max-uses <n>] [--no-owner]` mints a
  time-windowed, multi-claim code into a shared on-disk store; a running daemon
  picks it up on the next `/bind`/`/claim` with no restart. Also mintable by an
  owner from chat (owner-only `issue_pairing_code` tool) and from the TUI
  (`/pair`).
- **Universal `/bind` + `/claim`.** Self-onboarding now works on Telegram,
  Discord, Slack, Mattermost, Matrix, Signal, WhatsApp (Cloud + Web), IRC, Lark,
  DingTalk, QQ, Linq, Nextcloud Talk, and iMessage. `/bind <code>` grants chat
  access; `/claim <code>` also makes the sender an approval owner. Each channel
  keys on its native identity (numeric id / username / phone / contact) and
  appends to that channel's allowlist + `approval_owners`, effective immediately
  (no restart). Multiple owners can claim one code within its TTL window.
- **Gateway pairing on demand.** `--channel gateway` mints a code the gateway
  `POST /pair` accepts in addition to its startup code — add an API/console
  client without restarting the gateway.

### Security

- Pairing codes are surface-scoped (a code for one channel can't claim on
  another), SHA-256-hashed at rest in a `0600` store, and bounded by TTL +
  max-uses. `issue_pairing_code` is owner-only (`GuestGate::OWNER_ONLY_TOOLS`),
  so guests can't mint codes. No `config.toml` schema change.

## [0.6.81-alpha] — 2026-06-18

### Fixed

- **Changing a provider's API key in the console no longer 401s the other
  provider.** There was a single shared `api_key` (the active provider's key);
  switching the active provider in the console kept the old key, so e.g. an
  OpenAI request was sent with the MiniMax key → `401 invalid_api_key` ("rig
  openai completion failed"). Resolution also short-circuited on any non-empty
  key without consulting the provider's env var.

### Added

- **Per-provider API key store** (`provider_api_keys`, encrypted at rest like
  `api_key`; config schema v5→v6, additive). The console now saves each
  provider's key separately; switching the active provider carries that
  provider's key (and preserves the previous one). Credential resolution is now
  provider-aware (`Config::resolve_key_for_provider`): per-provider key →
  top-level `api_key` only for the active provider → provider-specific env var.
  Existing single-provider configs are unaffected.

## [0.6.80-alpha] — 2026-06-18

### Fixed

- **OpenAI: removed phantom model ids that 404'd.** The catalog pointed the
  `openai-codex` provider default at `gpt-5.5-codex` and listed `gpt-5.5-codex`
  and a bare `gpt-5.3` under `openai` — none of which exist on the OpenAI API
  (verified against `/v1/models` with a live key). Replaced with real ids:
  `gpt-5.3-codex` (newest codex that actually exists) is now the `openai-codex`
  default and recommended entry, and `gpt-5.4` replaces the bogus entries in the
  `openai` list. Added regression tests asserting the phantom ids never reappear.
  (The phantom `gpt-5.5-codex` predated the recent model refreshes — it was
  introduced in #45.)

## [0.6.79-alpha] — 2026-06-18

### Added

New flagship models across providers added to the curated catalog (setup wizard,
`/model` picker, provisioner). Model IDs were sourced from the `rig-core` model
constants where available and cross-checked against provider docs otherwise; the
providers below have no credentials in this environment, so IDs could not be
live-API-tested (unlike MiniMax-M3 in 0.6.78):

- **Anthropic**: `claude-opus-4-8` (rig-verified). Default stays `claude-sonnet-4-6`.
- **DeepSeek**: `deepseek-v4-pro` (new default) + `deepseek-v4-flash` (rig-verified).
- **Qwen**: `qwen3.7-max` + `qwen3.7-plus` (new default).
- **GLM / Z.ai**: `glm-5.2` (new default).
- **Moonshot**: `kimi-k2.7-code`. Default stays `kimi-k2.6`.

OpenAI (gpt-5.5), xAI (grok-4.1), Mistral, and Cohere were already current. GPT-5.6
(unreleased) and Claude Fable 5 (suspended) were intentionally excluded.

## [0.6.78-alpha] — 2026-06-18

### Added

- **MiniMax M3** is now the recommended MiniMax model. Added `MiniMax-M3` to the
  curated model catalog (setup wizard, `/model` picker, provisioner) and made it
  the default for the `minimax` provider, replacing `MiniMax-M2.7` (now listed as
  "previous flagship"). The ID was verified live against the MiniMax API; a
  `MiniMax-M3-highspeed` variant was checked and intentionally **not** added
  because the API rejects it (does not exist yet).

## [0.6.77-alpha] — 2026-06-18

Agent-authored skills: tell the bot "make me a skill that does X" and it
scaffolds a ready-to-use skill for you.

### Added

- **`author_skill` tool** — the write-side counterpart to the read/install
  skill tools. The agent creates a brand-new local skill from a plain-language
  request and writes a loader-valid `SKILL.md` into the active profile, where
  it loads on the next turn. Only `name` + `description` are required; the tool
  fills in sensible default instructions when none are given, so even a minimal
  call yields a complete, well-structured skill. Slugs are sanitized to a
  filesystem-safe form (no path traversal), an existing skill is never
  overwritten without `overwrite: true`, and the tool is approval-gated by name
  like `skills_install`.

## [0.6.76-alpha] — 2026-06-18

### Changed

- **Knowledge Base is now in the default build.** `kb` (sqlite-vec vector store
  + PDF/markdown/image ingestion) moved into the default feature set, so a
  classic install/build ships the `rantaiclaw kb` command (search / ingest /
  list / get / delete / drift / re-embed) without `--features kb`. Verified to
  cross-compile on all six release targets. Costs ~5MB of binary size (≈31MB);
  the release binary-size safeguard was raised one tier (30→35MB, advisory
  25→30MB) per the documented floor-history policy.

## [0.6.75-alpha] — 2026-06-17

Per-role channel permissions: owners get the full toolset; everyone else who
can chat is a "guest" under a configurable capability ceiling. Applies to every
multi-user channel, configurable via CLI, TUI, the onboarding wizard, or by
asking the bot in chat (owners only).

### Added

- **Guest capability ceiling** (`[channels_config] guest_allowed_tools` /
  `guest_allowed_commands`) — non-owner turns may use skills + read-only tools
  plus any allowlisted tools, and (for `shell`) only allowlisted command globs
  (e.g. `kubectl get *`). Out-of-ceiling calls are denied outright — never
  escalated to an owner — and the ceiling applies regardless of
  `autonomous_tools`. Enforced in the shared agent loop (`GuestGate`) on every
  polling channel and gateway/webhook channel; owners and the CLI/console
  operator are unrestricted. Config schema v4 → v5 (additive migration).
- **`rantaiclaw permissions`** CLI — `show`, `add`/`remove <owner|tool|command>
  <value>`; persists and reloads a managed daemon.
- **`/permissions`** TUI command (aliases `/perms`, `/owners`) — show or
  add/remove; the running runtime reloads on save.
- **Onboarding wizard** prompts for owners + the guest ceiling after a
  multi-user channel is configured.
- **`manage_permissions`** owner-only tool + bundled `owner-permissions` skill —
  owners can set ownership and the guest ceiling by asking the bot in chat.

### Security

- `manage_permissions` is hard-gated: `GuestGate::OWNER_ONLY_TOOLS` always denies
  it (and `delegate`/`ssh`/`pty`) for non-owners, regardless of the guest
  allowlist; refuses to remove the last owner from chat; serializes its writes.
- Guest shell commands reject all `$` (command substitution, `$VAR` env
  exfiltration, `$'…'` ANSI-C injection) and chaining/redirect/subshell/tab
  metacharacters before glob matching.

## [0.6.74-alpha] — 2026-06-14

Unified agent runtime: the TUI, CLI, channels, and gateway/console now share
one prompt builder and one agentic loop, with a single owner-gated approval
model across surfaces.

### Added

- **Unified approval model** — pluggable `ApprovalBackend` with all four
  surfaces wired: CLI prompt, auto-deny, **in-chat owner `/approve`** for
  polling channels (`ChatRelayApprovalBackend`), and an **in-browser modal**
  for the console SSE chat (`WebModalApprovalBackend` + `POST /api/v1/approvals/{id}`).
- **Owner-authority gate** (`can_approve` / `[channels_config] approval_owners`)
  — the requester is not automatically allowed to approve; secure-by-default
  (no owner ⇒ approval-required tools auto-deny).
- `PUT /api/v1/personality` now accepts the full persona (name/role/tone/avoid),
  not just the preset — enabling live persona switching from a console.
- Layered, conversation-scoped memory (`recall_layered` + `ConversationKey`).

### Changed

- Collapsed the two agent loops into one `run_structured_loop`
  (`ConversationMessage` + `ToolDispatcher`); channels/gateway/CLI/delegate use
  a thin adapter, behavior-preserving.
- Channel system prompts run through the same `SystemPromptBuilder` as the TUI,
  with surface-accurate Safety/preset text (owner approval, not the TUI's Y/N/A).
- Strict preset shell-filter parity applied across CLI, gateway, and channels.

## [0.6.40-alpha] — 2026-05-14

Approval policy preset rename: drop the `L1` / `L2` / `L3` / `L4`
prefixes and use the verbal labels directly (`Manual` / `Smart` /
`Strict` / `Off`). The `L1`–`L4` naming implied a hierarchy that did
not exist — `L4 — Off` reads counter-intuitive next to `L1 — Manual`,
and three of the four presets all map to the same runtime
`supervised` enum value. The new labels carry the meaning instead.

### Changed

- `PolicyPreset` enum variants renamed: `L1Manual` → `Manual`,
  `L2Smart` → `Smart`, `L3Strict` → `Strict`, `L4Off` → `Off`.
- On-disk preset identifiers changed: `preset = "L1"` … `"L4"` are now
  written as `preset = "manual"` … `"off"`.
- Preset bundle files renamed under `src/approval/presets/`:
  `policy_l1.toml` → `policy_manual.toml`, `policy_l2.toml` →
  `policy_smart.toml`, `policy_l3.toml` → `policy_strict.toml`,
  `policy_l4.toml` → `policy_off.toml`.
- Setup wizard dropped the `L1 —` / `L2 —` / … prefix; menu now reads
  `Manual — prompt for every tool call`, etc.
- `Off` preset (formerly `L4`) warning text updated to use the new
  label.
- Pillar docs (`docs/pillars/{1-setup,3-tools-approvals}.md`) and
  README autonomy section updated to use the verbal preset names.

### Compatibility

- `PolicyPreset::from_str_ci` continues to accept the legacy
  `L1`/`L2`/`L3`/`L4` ids so existing `<profile>/policy/autonomy.toml`
  files written by pre-v0.6.40 releases still load without
  hand-editing.
- The setup wizard rewrites the file with the new label on next
  `setup approvals --force`.

## [0.6.8-alpha] — 2026-05-07

UI consistency cut driven by the v0.6.7 tester recommendation: *"Change
the shitty on chat ui or infos to proper tui comp ui."* Seven info
commands now open dedicated TUI panels instead of dumping `System:`
chat blobs. Three picker/wizard polish fixes. One alias dropped.

### Added

- **`InfoPanel` widget** at `src/tui/widgets/info_panel.rs` — bordered
  modal matching the `list_picker` visual language: sky-bold title,
  optional subtitle, sectioned typed rows (`KeyValue` / `Status` / `Bullet`
  / `InlineList` / `Plain` / `Spacer`), scrollable when content overflows,
  Esc closes, ↑/↓/PgUp/PgDn scroll. Brand colors stay in sync with
  `list_picker` and `setup_overlay` so the surfaces feel like one app.
- New `CommandResult::OpenInfoPanel(InfoPanel)` variant; `TuiApp.info_panel`
  field; render integration in both inline and fullscreen paths;
  alt-screen toggle picks up the panel.

### Changed

- **`/channels`** — text-blob → InfoPanel. Sections: Always available /
  Runtime (auto-start state) / Configured (per-channel status with the
  same icon-vocabulary as `/doctor`) / Not configured (compact comma-
  list) / Logs.
- **`/config`** — text-blob → InfoPanel. Sections: Runtime / Persisted
  with pointer at `~/.rantaiclaw/profiles/<active>/config.toml`.
- **`/doctor`** — text-blob → InfoPanel + content expansion. Was
  3 trivial checks (session store, model, TUI); now adds Channels
  (auto-start state + each configured channel), Skills (count loaded),
  Workspace (`~/.rantaiclaw/`, `profiles/`).
- **`/insights`** — text-blob → InfoPanel. Sections: Sessions (total +
  current age) / Messages (total + current + per-session avg) / Tokens
  (this session).
- **`/status`** — text-blob → InfoPanel. Sections: Agent / Session.
- **`/usage`** — text-blob → InfoPanel. Sections: Tokens / Model
  (active + context window).
- **`/skill`** (no args) — text-blob → InfoPanel listing all loaded
  skills with descriptions; usage hint section. `/skill <name>` opens
  a per-skill detail panel.

### Polish

- **First-run wizard welcome footer**: "skip any step with Esc" → "Esc
  to cancel". Esc on Welcome quits the wizard (there's nothing to skip
  yet); the wording was misleading. Mid-step screens still say "skip"
  which is correct semantics there.
- **List picker cross-page navigation**: pressing ↓ at the last item
  of a page now advances to the first item of the next page (and
  symmetric for ↑ at the first item of a non-first page). Pre-v0.6.8
  ↓ wrapped to row 1 of the same page, leaving testers stuck on
  page 1 of 3 in the ClawHub picker without realizing PgDn was
  required to advance.
- **`/personality` picker** now opens on the actual current preset and
  marks that row with `· current` in the secondary line. Pre-v0.6.8
  the picker hardcoded `Some("default")` as the preselect, ignoring
  whatever was actually saved in `<profile>/persona/persona.toml`.

### Removed

- **`/platforms` alias** — was a v0.6.4 alias for `/channels` for
  muscle memory, but tester feedback flagged the duplicate output as
  noise. The single canonical command is `/channels`.

### Compatibility

- No on-disk-state changes. No new deps.
- `CommandResult::OpenInfoPanel(...)` is additive; existing callers
  using `Message(...)` continue to render as inline chat lines.

## [0.6.7-alpha] — 2026-05-07

Two TUI fixes from v0.6.6-alpha tester feedback. One UX gap deferred
(ClawHub picker default selection) for v0.6.8.

### Fixed

- **Channel events leak into the local TUI scrollback** — every incoming
  channel message ("[telegram] from @user: ..."), the "Processing
  message..." progress line, every reply ("Reply (4208ms): ..."), and
  delivery failures were `println!`/`eprintln!` to stdout. In TUI mode
  stdout is the alt-screen, so the channel chatter corrupted the
  rendering and exposed Telegram conversations the local user wasn't
  meant to see. v0.6.7 routes all four through `tracing::info!` /
  `tracing::error!` instead. Channel activity is now visible only in
  `~/.rantaiclaw/logs/tui-YYYY-MM-DD.log`. Daemon-mode operators who
  relied on stdout for live message tracing should now `RUST_LOG=info`
  + tail the log file. (`src/channels/mod.rs` lines ~1324, ~1374,
  ~1563, ~1589.)

### Added

- **Restart-needed cue when channels are added/removed mid-session** —
  `reload_config` now compares channel count before vs after and pushes
  a `⚠` system message into chat scrollback if it changed. Tester report
  was "Telegram works only after restarting `rantaiclaw`" — true, and
  the cue makes the requirement visible. Auto-restart of the
  `start_channels` task is the v0.6.8 follow-up; doing it cleanly needs
  `start_channels` to accept a cancellation token to avoid orphaning
  supervised listener tasks.

### Deferred

- ClawHub picker installs 0 skills despite "✓ Installed from ClawHub"
  banner. Picker likely defaults to nothing-selected; user pressed
  Enter without picking. Fix is a UX adjustment in `src/onboard/section/skills.rs`
  — either default-check top-3 skills or warn on empty selection. Will
  land in v0.6.8.

### Compatibility

- No on-disk-state changes. No new deps.

## [0.6.6-alpha] — 2026-05-07

Diagnostic upgrade for the channel auto-start path. Tester reported v0.6.5
showed "polling" for Telegram in `/channels` but the bot still didn't
reply — meaning the dispatch happened but `start_channels` either errored
mid-build or the listener silently failed. Pre-v0.6.6 had no way for the
user to see what went wrong; the warn was logged to a file the user
didn't know to check.

### Added

- `src/channels/auto_start_state.rs` — global Mutex<AutoStartState> with
  variants `NotDispatched`, `Starting`, `Terminated`, `Failed{message}`.
  The TUI auto-start callback marks the state through the spawn lifecycle.
- `/channels` now reads the snapshot and shows one of:
  - `running` — start_channels is past the build phase and likely in the
    dispatch loop
  - `starting…` — within the first 5 seconds of startup
  - `FAILED — see error below` + the formatted error chain
  - `stopped (dispatch loop exited)`
  - `configured · not started in this process`
- Footer hint at the bottom of `/channels` always points at
  `~/.rantaiclaw/logs/tui-YYYY-MM-DD.log` for full provenance.

### Fixed

- `/channels` no longer reports `polling` when the spawn task errored.
  Tester report: "Telegram still not working even though it reports to
  polling." Status was misleading; this gives an honest answer.

### Compatibility

- No on-disk-state changes. No new deps.

## [0.6.5-alpha] — 2026-05-07

Build-pipeline fix. v0.6.3-alpha and v0.6.4-alpha binaries reported their
version as `0.6.2-alpha` because Swatinem/rust-cache was restoring a
target/ from a previous v0.6.2-alpha build, and cargo's incremental
compilation didn't re-emit `main.rs` with the new `CARGO_PKG_VERSION`
even though Cargo.toml had been bumped. The binaries were otherwise
identical functionally — they had all the v0.6.3 + v0.6.4 fixes — but
the version string was wrong, which testers flagged as confusing.

### Fixed

- **Wrong `--version` output on alpha builds** — `pub-release.yml` now
  runs `cargo clean -p rantaiclaw --target <target>` between the cache
  restore step and the build step. This invalidates only the
  `rantaiclaw` package's incremental-compilation fingerprint while
  leaving dependency builds cached, so `env!("CARGO_PKG_VERSION")`
  re-expands fresh against the current `Cargo.toml`. Build-time impact:
  ~30-60 sec extra on cache-warm runs (the rantaiclaw crate recompiles
  from scratch instead of incrementally). Negligible on cache-cold
  runs. (`.github/workflows/pub-release.yml`.)

### Compatibility

- **No source code changes.** This is a CI-only fix. The shipped binary
  is functionally identical to v0.6.4-alpha; only the `--version` output
  is corrected. If you trust your v0.6.4-alpha build is doing the right
  thing functionally (e.g. Telegram replies), it's the same code.

## [0.6.4-alpha] — 2026-05-07

Follow-up to v0.6.3-alpha tester feedback. Fixes the channel deadlock
(Telegram bot configured but never replied), makes the channel state
visible, and lands the deferred back button.

### Fixed

- **Telegram / Discord / Slack / etc. don't reply when running bare
  `rantaiclaw`** — the TUI process was the canonical "all-in-one"
  runtime in user expectations, but it was only running the local-chat
  agent actor. Configured channels needed a separate `rantaiclaw daemon`
  to be polled, which wasn't documented or discoverable. v0.6.4 spawns
  `start_channels` as a background task alongside the TUI when any
  channel is configured. Failure-mode discipline: channel-startup
  errors are logged but don't crash the TUI; the user can still chat
  locally. (`src/tui/app.rs` `run_tui`.)
- **`/platforms` always reported "TUI active" only** — was a hardcoded
  string. Now reflects the actual `channels_config` from
  `<profile>/config.toml` and indicates whether each channel is
  configured + whether the TUI is polling it. Refreshes on
  `reload_config` so post-`/setup` runs see the new state.
  (`src/tui/commands/config.rs`.)

### Added

- **`/channels` command** — first-class command to list configured +
  active channels. `/platforms` now aliases to `/channels`.
- **Back button in the first-run wizard** — `Ctrl+B` walks the phase
  history one step back. Safe cases (PickChannels ↔ PickIntegrations
  ↔ Welcome) work fully. RunningProvisioner steps are skipped on
  rewind (the running task wrote to `config.toml`; surgical rewind
  isn't safe). For redoing a required section, `Esc` + re-run with
  `/setup <section>` remains the supported path. (`src/tui/first_run_wizard.rs`
  `back()`, `src/tui/app.rs` Ctrl+B handler.)

### Compatibility

- No new deps. No on-disk-state changes vs v0.6.3-alpha.
- Bare `rantaiclaw` now uses more memory + CPU when channels are
  configured (it spawns the channel listeners). For TUI-only mode
  with no channels, the auto-start branch is a no-op.

## [0.6.3-alpha] — 2026-05-07

Bug-fix cut driven by Sulthan + Alifia's first round of v0.6.1-alpha
testing (`bugs-123.pdf`). Five tester-reported bugs fixed; one
deferred (back-button navigation needs wizard rework).

### Fixed

- **Provider 401 immediately after `/setup provider`** — `reload_config`
  was reading the encrypted `config.toml` and pushing it straight to
  the agent actor without running the secret-decrypt pass that
  `Config::load_or_init` performs at startup. The agent received an
  encrypted blob in `config.api_key`, the HTTP request builder rejected
  the malformed Authorization header, and OpenRouter / OpenAI
  responded "401 Unauthorized: Missing Authentication header". Reload
  now runs the same `decrypt_optional_secret` pass for `api_key`,
  `composio.api_key`, `browser.computer_use.api_key`,
  `web_search.brave_api_key`, `storage.provider.config.db_url`, and
  every `agents.*.api_key`. (`src/tui/app.rs` `reload_config`)
- **`/skills` shows "No skills loaded" even after the starter pack
  installed** — v0.5.0 introduced a per-profile skills dir
  (`<profile>/skills/`) but the loader still looked at the v0.4.x
  workspace-level path (`<workspace>/skills/`). The bundled installer
  + ClawHub both write to the new path; the picker was reading from
  the old one. Loader now checks both, profile-level wins on conflict,
  deduped by name. Empty-state hint corrected to point at the actual
  v0.5.0+ path. (`src/skills/mod.rs` `load_workspace_skills`,
  `src/tui/commands/skills.rs`)
- **`/skill` and `/skills` produced identical output** — both opened
  the same picker. `/skill` (no args) now prints usage + an inline
  list of loaded skills; `/skills` keeps the interactive picker.
  `/skill <name>` unchanged. (`src/tui/commands/skills.rs`)
- **`/resume` shows "Resumed session ... (N messages)" but no
  history** — messages were loaded into `context.messages` but never
  pushed into the scrollback display queue. The user saw a fresh-looking
  TUI even though the agent had the history. Resume now replays each
  loaded message into `scrollback_queue` so the conversation actually
  appears. (`src/tui/app.rs` `ListPickerKind::Session` arm)
- **ClawHub install fails with 404 on auxiliary files** — a stale
  upstream `README.md` reference in a manifest was breaking the entire
  install. SKILL.md remains required (a skill without it is rejected
  per the bundled-format contract); other files (README, LICENSE, etc.)
  are now best-effort with a `tracing::warn!` on 404. (`src/skills/clawhub.rs`)

### Deferred

- **No back button in the wizard / setup picker** — substantial state-
  machine work to add reverse navigation across the seven setup steps.
  Filed as a follow-up; for now testers can `Esc` to cancel and re-run
  the section.

### Compatibility

- No new deps. No on-disk-state changes vs v0.6.2-alpha.
- Skills installed under the v0.4.x `<workspace>/skills/` layout still
  load (back-compat path retained).

## [0.6.2-alpha] — 2026-05-06

Lifecycle commands — closes the "how do I uninstall?" / "how do I update?"
completeness gap surfaced during v0.6.1-alpha tester onboarding. New module
`src/lifecycle/` with two commands.

### Added

- **`rantaiclaw uninstall`** — remove profile data, optionally the binary.
  Default scope is the active profile only; `--all` removes the entire
  `~/.rantaiclaw/` tree; `--purge` adds binary self-deletion. `--keep-secrets`
  preserves `.secret_key` for re-install. `--dry-run` prints the plan and
  exits 0. Coordinates with the daemon service unit (calls `service uninstall`
  automatically when present). Best-effort: comments out PATH amendments the
  installer may have added to `~/.bashrc` / `~/.zshrc` / `~/.profile` /
  `~/.config/fish/config.fish` with a date-stamped marker.
- **`rantaiclaw update`** — self-replace the binary against a published
  GitHub release. Verifies SHA256 against `SHA256SUMS`. Atomic swap on
  Unix (rename + rename, with `.old` backup and rollback on failure). On
  Windows, stages the new binary as `<exe>.new.exe`; the next launch
  detects and self-swaps before doing anything else.
  Flags: `--check`, `--channel stable|prerelease`, `--to <tag>`,
  `--allow-downgrade`, `-y/--yes`. Honors `RANTAICLAW_RELEASE_BASE_URL`
  for testing against staging or self-hosted releases.
- Refuses self-modification on cargo-managed binaries (`~/.cargo/bin/`)
  with a hint to use `cargo install rantaiclaw --force` or
  `cargo uninstall rantaiclaw` instead.

### Changed

- `src/main.rs` short-circuits `Update` and `Uninstall` before
  `Config::load_or_init` so they work on partially broken installs.
- Every launch applies a pending Windows update before doing anything
  else (no-op on Unix; cheap stat call on Windows).

### Compatibility

- **No deps added.** Implementation reuses existing `reqwest`, `sha2`,
  `hex`, `tempfile`. Archive extraction shells out to `tar` (Linux/macOS
  native, ships in Windows 10 1803+). This keeps the Cargo.toml dep
  surface unchanged from v0.6.1-alpha — a deliberate alignment with the
  bloat-audit "no new deps for one feature" rule.
- Configs and on-disk state from v0.6.1-alpha load unchanged.

### Tests

- `src/lifecycle/binary_path.rs` — InstallKind classification + cargo
  refusal.
- `src/lifecycle/uninstall.rs` — dry-run, default-active-profile-only,
  `--all` full wipe, `--keep-secrets` preserves `.secret_key`, shell rc
  amendment is commented out (not deleted).
- `src/lifecycle/update.rs` — semver comparison incl. prerelease ordering,
  SHA256SUMS line parsing for multiple formats, tag normalization.

## [0.6.1-alpha] — 2026-05-06

Alpha cut for **Sulthan + Alifia** to start E2E testing on real hardware. No
runtime behavior changes vs v0.5.3; this is a docs + PM-alignment build that
ships the first feature-grouped task structure under v0.6.0 — Product
Completeness Beta in ClickUp.

### Added

- **9 pillar docs** under `docs/pillars/` — one per product surface, with
  competitor-parity matrix vs OpenClaw + Hermes-agent, maturity table,
  architecture diagram, trait extension point, CLI/config, and roadmap.
  Pillar 1 = Setup, 2 = Providers, 3 = Tools/Approvals, 4 = Skills/MCP,
  5 = Channels, 6 = Memory/Profiles, 7 = Gateway/Daemon, 8 = Install/Release,
  9 = Documentation.
- **`docs/project/codebase-bloat-audit-2026-05-06.md`** — date-stamped
  source-code bloat audit (top 10 findings, dep hit list, module split
  candidates, niche features to feature-gate).
- **`docs/project/docs-cleanup-plan-2026-05-06.md`** — proposed lean docs
  structure aligned with ClickUp pillars (Phase A delivery).

### Changed

- `docs/README.md` rewritten as a 30-second decision-tree entry hub. Dropped
  the multilingual mirror claim that was never implemented.
- `docs/SUMMARY.md` rewritten as a unified TOC reflecting the new IA
  (start / reference / pillars / operations / security / hardware /
  contributing / project).
- `CLAUDE.md` §4.1 amended to drop EN/ZH/JA/RU parity claim and document the
  new pillar-anchored doc system + archive convention for superseded planning
  artifacts.

### Moved

- `docs/superpowers/{plans,specs}/*` (9 files, ~7,600 LoC of dated planning
  artifacts that were superseded by shipped releases v0.5.0–v0.5.3) →
  `docs/project/archive/superpowers/{plans,specs}/`. Content preserved per
  CLAUDE.md §4.1 archive convention.

### Compatibility

- **No runtime changes.** Configs and on-disk state from v0.5.3 load
  unchanged.
- Binary version string is now `0.6.1-alpha`; downstream tooling that
  pin-matches on `0.5.x` will need to widen the pattern.

## [0.5.3] — 2026-04-30

Model-default refresh — every provider's wizard menu and quick-mode
default now points at the actually-current generation. Older
generations stay in the menus as fallbacks for users on older API
tiers (with explicit `(previous flagship)` / `(legacy)` suffixes).

### Changed

- **OpenAI**: `gpt-5.2` → `gpt-5.5`. Codex variant: `gpt-5-codex` →
  `gpt-5.5-codex`. Menu adds gpt-5.3 / 5.2 / 5.1-codex-mini as
  fallbacks. (#45)
- **Anthropic**: `claude-sonnet-4-5-20250929` → `claude-sonnet-4-6`.
  Menu adds `claude-opus-4-7` and keeps `haiku-4-5` + legacy
  `sonnet-4-5`. (#40 → #45 verified)
- **Gemini / Google**: `gemini-2.5-pro` → `gemini-3-pro`. Adds
  `gemini-3-flash`; previous tier kept. (#45)
- **Moonshot / Kimi**: `kimi-k2.5` → `kimi-k2.6`. Older revisions stay
  in the menu. (#45)
- **GLM / Z.AI**: `glm-5` → `glm-5.1`. (#45)
- **MiniMax**: `MiniMax-M2.5` → `MiniMax-M2.7` (with M2.7-highspeed
  variant). (#45)
- **Qwen**: `qwen-plus` → `qwen3.6-plus`; coder track moves to
  `qwen3.6-coder-plus`. (#45)
- **Meta / Llama family** (groq, fireworks, together-ai, nvidia,
  llamacpp): default flips from the `llama-3.3-70b` family to
  **`llama-spark`** — Meta's latest generation. (#45)
- **OpenRouter / Venice / Astrai** curated lists pick up
  cross-provider entries: `gpt-5.5`, `gpt-5.5-codex`, `gemini-3-pro`,
  `gemini-3-flash`, `kimi-k2.6`, `glm-5.1`, `meta-llama/llama-spark`.
  (#45)
- **Bedrock**: `…sonnet-4-5-20250929-v1:0` → `…sonnet-4-6-v1:0`. (#45)

### Compatibility

- **Backward compatible.** Explicit `--model` overrides and existing
  configs are honored unchanged. Only the *next* `onboard` run picks
  up the new default for users who didn't pin one.
- DeepSeek left at V3.2 pending confirmation on the newer model id —
  if you have it, drop the name and the wizard wires it in next.

## [0.5.2] — 2026-04-29

Setup-flow polish + visual fixes from a real end-to-end audit. Every
fix is testable today; the audit doc that drove the batch lives at
`docs/superpowers/specs/2026-04-29-setup-audit.md`.

### Added

- **Hermes-style splash for `agent` + `setup`** — `RANTAICLAW` rendered
  in chunky ANSI Shadow figlet at the top, brand logo as 30×16 Braille
  pixel art on the left of a framed pane, gradient sky → blue → navy →
  muted colours from the rantai-agents web palette (#5eb8ff / #3b8cff /
  #040b2e / #6b7280). Adapts to terminal width: full splash at ≥80
  cols, smaller `slant` font fallback at ≥60, single-line banner
  below that. (#40)
- **Polished interactive TUI** — empty-chat splash, Hermes-style
  bottom status bar (`$ model │ tokens/window pct% │ N msgs │ session
  age`), Braille spinner during streaming, rounded brand borders, sky
  highlight on the input prefix `▎ $ you`, `Rantaiclaw v… · session
  …` header chip. (#40)
- **Slash-command autocomplete dropdown** — pops up the moment the
  input buffer starts with `/`, filters by prefix on every keystroke.
  Two-column layout: command name in sky-blue, description in muted
  gray, truncated with `…`. Tab completes; Enter completes-then-submits;
  Esc dismisses; ↑/↓ navigates. (#40)
- **`/help` modal overlay** — Claude-Code-style centered panel with
  `general` + `commands` tabs (Tab to switch, Esc to close), sky-blue
  active-tab chip, brand-coloured shortcut grid, repo URL footer. (#40)
- **WhatsApp Web QR is now actually scannable** — new
  `src/channels/qr_terminal.rs` renders `Event::PairingQrCode`
  payloads as Unicode block-character QR via the `qrcode` crate
  (added as a hard dep), framed and printed to stderr with header +
  footer. Works on any modern terminal; user can point a phone at it
  during pairing. Same module formats the human-readable pair-code
  flow. (#41)
- **Doctor `channels.auth` runs real probes** — Telegram `getMe`,
  Discord `users/@me`, Slack `auth.test`, WhatsApp Cloud
  `/v18.0/<phone_id>`, WA Web session-DB existence. 5s timeout per
  channel. `--offline` falls back to the original config-only sanity
  scan. (#42)
- **MCP zero-auth servers validate at setup time** — same
  `validate_mcp_startup` spawn-and-wait the authed branch already had,
  so a missing `npx`/`uvx` surfaces during `setup mcp` instead of at
  first agent run. (#42)
- **Approval-preset round-trip self-check** — after writing
  `autonomy.toml` / `command_allowlist.toml` / `forbidden_paths.toml`,
  the writer re-reads each freshly-written file and verifies the
  schema. Catches preset-bundle drift, schema mismatches, and
  encoding bugs at setup time. Files the call did NOT freshly write
  (idempotent no-op on user-edited content) are skipped. (#42)
- **Setup-time warning when WhatsApp Web is selected without
  `--features whatsapp-web`** — yellow warn block + rebuild
  instructions + explicit confirmation prompt (default no), so users
  cannot silently configure something that won't run. (#42)

### Fixed

- **`.secret_key` migration left encrypted api_keys un-decryptable** —
  v0.5.0's flat-to-profile migration moved `config.toml` but left
  `~/.rantaiclaw/.secret_key` behind; the SecretStore derived its key
  path from `config_path.parent()` and spawned a fresh key in the
  profile dir, leaving the encrypted blob un-decryptable on next
  launch. Migration's movables list now includes `.secret_key`,
  `secrets/`, and defensively `.onboard_progress`. (#39)
- **ClawHub install was always writing a stub SKILL.md** — old
  `install_one` looked for a `latestVersion.readme` field that the
  live API does not expose, so every "install from ClawHub" produced
  a one-line placeholder. New three-step fetch walks
  `GET /skills/:slug → version` → `GET /skills/:slug/versions/:v →
  files[*]` → per-file fetch with SHA-256 verify against the
  manifest. Path-traversal guard, capped exponential 429 backoff,
  partial-install cleanup on failure. (#41)
- **`ClawHubSkill.tags` shape mismatch** — upstream returns
  `{ "latest": "x.y.z" }`, Rust expected `Vec<String>`,
  `serde(default)` silently emptied it. Retyped to
  `serde_json::Value`. (#41)
- **Plain Enter wasn't submitting prose in the TUI** — the input
  handler only submitted on slash-command buffers; for prose it
  inserted a newline silently, leaving users unable to send a message
  on terminals that don't pass `Ctrl+Enter` as a Ctrl-modified
  KeyCode::Enter. Now plain Enter always submits; multi-line via
  Ctrl+J or Shift+Enter. (#43)
- **`tracing::warn!` from agent path corrupted the TUI alt-screen** —
  no subscriber was installed for the bare `rantaiclaw` launch path,
  so tracing fell through to default-stderr and warnings interleaved
  with the chat. Routed to `~/.rantaiclaw/logs/tui-YYYY-MM-DD.log`
  via a per-launch file writer. `RUST_LOG` still honoured. (#43)
- **`hardware`/`peripheral-rpi`/`probe` features failed to compile**
  because `firmware/rantaiclaw-arduino/zeroclaw-arduino.ino` was the
  legacy filename. Renamed to match the rust `include_str!` path. (#43)

### Changed

- **Friendlier chat-side errors when an agent turn fails** — the
  TUI's `finalize_error` now recognises common shapes (API key
  missing, rate limit, model unavailable) and rewrites them into a
  short actionable block. Unknown errors fall through verbatim with
  the multi-attempt tail compacted to "+N more attempts". (#43)
- `RenderTheme::default()` now uses the rantai-agents brand palette
  (sky / blue / mint / coral / muted) instead of generic 8-colour
  ANSI, so the TUI matches the splash. (#40)
- `CommandResult::Message` outputs now land in chat history (as a
  `system`-role message) instead of the one-line status bar slot.
  Previous behaviour silently truncated long output and disappeared
  on the next keystroke. (#40)

### Compatibility

- All v0.5.0 / v0.5.1 configs continue to load; the migration fix in
  #39 only affects users who hadn't yet hit the bug.
- `--all-features` builds **except** `channel-matrix` (matrix-sdk 0.16
  hits Rust's recursion limit; needs an upstream `#![recursion_limit]`
  bump). The exclusion is documented inline in `Cargo.toml`.

## [0.5.1] — 2026-04-28

Linux portability hotfix.

### Fixed

- **Release Linux binaries built against GLIBC 2.35 baseline** — v0.4.x and
  v0.5.0 release artifacts were built on `ubuntu-latest` (Ubuntu 24.04, GLIBC
  2.39) and refused to load on Ubuntu 22.04 LTS, Debian 12, RHEL 9, and most
  other LTS distros with `libc.so.6: version 'GLIBC_2.39' not found`. All
  three Linux runners (x86_64, aarch64, armv7) are now pinned to
  `ubuntu-22.04` so artifacts run on every modern distro from Ubuntu 22.04
  onward. (#38)

## [0.5.0] — 2026-04-28

The "onboarding depth v2" release. The setup story is now modular,
re-runnable, and policy-aware end to end. Existing flat-layout
configurations migrate automatically on first run.

### Added

- **Profile system** (Wave 1) — multi-profile storage layout under
  `~/.rantaiclaw/profiles/<name>/`. New `rantaiclaw profile {list, create,
  use, clone, delete, current}` subcommands plus a global `--profile <name>`
  flag (precedence: CLI flag > `RANTAICLAW_PROFILE` env > `active_profile`
  file > `default`). Legacy single-profile installs auto-migrate on first
  load via `ProfileManager::ensure_default`.
- **Approval gate + audit log** (Wave 2A) — every tool call now flows
  through a single approval policy gate (`src/approval/`) before
  execution. Policy combines an autonomy mode, a command allowlist, and a
  forbidden-paths list; decisions are written to a tamper-evident audit
  log under `<profile>/audit/`. Security tests cover bypass, path
  escapes, and approval-gate edge cases.
- **`rantaiclaw doctor`** (Wave 2B) — diagnostics across config,
  policy, system deps, and daemon registration. Three output formats
  (`text`, `json`, `brief`); each finding ships with an actionable hint.
- **Persona presets + interview** (Wave 2C) — five curated presets
  (default, executive-assistant, friendly-companion, research-analyst,
  concise-pro) plus an interactive interview that renders `persona.toml`
  and a `SYSTEM.md` prompt. Snapshot tests lock the rendering.
- **Skills starter pack + ClawHub** (Wave 2D) — 5-skill bundled starter
  pack (web-search, scheduler-reminders, summarizer, research-assistant,
  meeting-notes) installed in headless mode; ClawHub multi-select picker
  for additional skills, sorted by stars.
- **MCP curated picker** (Wave 2E) — 9 vetted servers (3 zero-auth,
  6 authenticated) with inline auth flow during setup.
- **Setup orchestrator** (Wave 3) — `rantaiclaw setup [<topic>] [--force]
  [--non-interactive]` walks the canonical six-section list (provider →
  approvals → channels → persona → skills → mcp) or dispatches to a
  single section. `--non-interactive` makes every section emit a hint
  and continue, suitable for CI / scripted bootstraps.
- **L1-L4 policy presets** (Wave 4A) — `rantaiclaw setup approvals`
  picks between L1 Strict, L2 Smart, L3 Trusted, and L4 Auto presets;
  per-agent overrides supported via `[agents.<name>.autonomy]`.
- **Daemon handoff on profile switch** (Wave 4B) — `profile use` now
  signals a running daemon to drain and re-launch under the new
  profile via a sentinel file; daemon lifecycle hooks write/clear it
  on start/stop.
- **OpenClaw / ZeroClaw migration** (Wave 4C) — `rantaiclaw migrate
  --from {openclaw, zeroclaw, auto}` imports config + workspace from a
  legacy install into a fresh profile. `--include-secrets` is opt-in;
  the source is never deleted.
- **End-to-end smoke tests** (Wave 5) — `tests/setup_e2e.rs` drives the
  compiled binary through `setup --non-interactive` and `doctor --brief`
  against a temp `$HOME`, asserting every section runs and the doctor
  surfaces the expected gaps.

### Changed

- `rantaiclaw onboard` is now a legacy alias for `rantaiclaw setup`. It
  prints a one-line `note:` and continues to work through v0.5.0; new
  recipes should use `setup`.
- `scripts/bootstrap.sh` post-install banner now points at `rantaiclaw
  setup` (and `rantaiclaw doctor`) instead of `rantaiclaw onboard
  --interactive`.

### Breaking

- **Storage layout migration.** Configs and workspace files move from
  the flat `~/.rantaiclaw/{config.toml, workspace/, ...}` layout to the
  per-profile `~/.rantaiclaw/profiles/<name>/{config.toml, workspace/,
  ...}` layout. The migration is automatic on first run; the old paths
  are left in place as compatibility symlinks for at least one release.
- The canonical setup section list grew from five to six (added
  `approvals` between `provider` and `channels`). Tests pinning the
  list size or order need to be updated alongside.

### Compatibility

- v0.4.x configs auto-migrate on first run (`tests/compat_v041_to_v050.rs`,
  `tests/migrate_legacy.rs`).
- All Wave 2-4 security tests pass; the approval gate is the only path
  between LLM tool emission and execution.
