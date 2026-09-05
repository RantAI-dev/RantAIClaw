# Changelog

All notable changes to RantaiClaw are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Removed

- **Three modules the product advertised and could not run.** `src/runtime/wasm.rs`
  (687 lines, 36 tests) was never declared as a module, has no `runtime-wasm` feature
  behind it, and imported a `WasmRuntimeConfig` type that does not exist — it could not
  compile, and `create_runtime` has no `wasm` arm. `src/observability/broadcast.rs` was
  likewise undeclared, with no `broadcast` arm in `create_observer`. `src/skillforge/`
  (1,118 lines) did compile, but `mod skillforge;` in `src/main.rs` was its only
  reference anywhere: no CLI verb, no config key, no caller.

### Changed

- **README and pillar 4 now name the sets that exist.** `[runtime].kind` accepts
  `native` or `docker` (not `wasm`); the observability backends are OpenTelemetry,
  Prometheus, log and noop (not `broadcast`); skill authoring is the `author_skill`
  tool (not `skillforge`).

## [0.29.0-alpha] — 2026-09-05

Wave 1 of the production-readiness audit plus the follow-up batch it produced:
fourteen fixes, none of which had reached a user. Five are security fixes (MCP
server credentials written to disk in the clear, credentials reaching the memory
store and cron announcements unredacted, a rate limiter keyed on an unvalidated
bearer, release signature verification that failed open), one is a session-killer
(history trimming split tool-call pairs, so every later turn in a tool-heavy
session got a 400), and one turned prose into outbound requests (a bare URL in a
model's reply became a `shell` `curl`). The rest cover headless setup honesty,
the gateway reading the config it is actually running, cron tool schemas, and one
answer to "can this provider send?" shared by every surface that asks.

**Config schema v27 → v28**: `[mcp_servers.<name>].env` values are encrypted at
rest, so this release does **not** roll back cleanly to 0.28.0-alpha — a v28
config refuses to load on a pre-v28 binary with `schema_version=28 is newer than
this binary supports`. The migrate arm itself transforms nothing; the one-time
re-encryption happens in `Config::load_or_init` during the write-back the version
bump triggers. Keep a copy of `config.toml` before first launch if you may need
to go back. claw-ui pin bumped to **v0.3.26**.

Five behaviour changes need an operator's attention:

- **`update` can now refuse on a machine without cosign.** Signature verification
  no longer falls back to a SHA-256 check against a checksum file served from the
  same origin as the archive. If cosign is missing or verification cannot run,
  the update stops; pass `--allow-unverified` to proceed knowing the artifact was
  not verified.
- **`setup <topic> --non-interactive` now exits non-zero when it configures
  nothing.** It previously printed "nothing saved" and returned 0, and saved the
  config anyway. An installer or CI job that ignores exit codes will start seeing
  failures where it previously saw silent success — including a provider section
  that ends with no credential the agent could send with.
- **`save()` no longer bakes environment values into `config.toml`.** A setting
  that arrived from the environment for one run — `RANTAICLAW_ALLOW_PUBLIC_BIND`,
  `RANTAICLAW_API_KEY` — is no longer made permanent by the first console, TUI or
  setup write. Configs already carrying such a value keep it; this stops new ones
  being created.
- **The gateway's API rate limiter is keyed after authentication.** A caller
  presenting an unrecognised bearer now shares the bucket for its network
  identity instead of getting a fresh one per invented token. With
  `trust_forwarded_headers` enabled, the client address is taken from the
  right-most `X-Forwarded-For` hop (the one your proxy appended) rather than the
  left-most (the one a client can write).
- **MCP server `env` values are encrypted on save.** They are decrypted by the
  same authority that governs every other stored credential, so `McpClient`
  still spawns with the plaintext it needs. Reading `config.toml` by hand or with
  another tool will show ciphertext where tokens used to be.

No exposure-boundary default is widened.

### Security

- MCP server credentials no longer sit on disk in the clear. Notion, Slack and
  GitHub tokens configured under `[mcp_servers.<name>].env` bypassed
  `decrypt_config_secrets`, the documented single authority on which fields are
  encrypted. The config API redacts them on the wire, which is what made the gap
  easy to miss — the exposure was the file itself. Every value is treated as a
  secret rather than guessed from the variable name, because a name-shaped
  heuristic misses `DATABASE_URL` and `PGPASSWORD`. Carries config schema v28.

- Credentials stop leaving the process unredacted on two paths. Auto-save: four
  call sites screen a message before storing it as memory, and only the channel
  dispatcher did — the agent loop's two sites and `Agent::turn` did not. Memory
  is the one store read back into a later prompt without anyone looking at it
  again, so a credential typed into the TUI landed verbatim in `brain.db`, was
  returned by `memory_recall`, was served by `GET /api/v1/memory`, and travelled
  back to the provider on every later recall. All four now go through one
  `memory::autosave_screened`. Cron announcements were the second path.

- The gateway's API rate limiter is keyed on an authenticated principal. The
  middleware runs before authentication, so the bucket came from whatever bearer
  the request presented: a caller with no valid token got a fresh bucket for
  every token they invented, and the key map could be made to churn. When
  `require_pairing` is false that limiter is the only guard in front of
  `agent/chat`, which spends money per request. A token becomes a principal only
  after it checks out against the same paired-token set the auth layer uses.

- Client addresses are read from the right proxy hop. With
  `trust_forwarded_headers` enabled, `X-Forwarded-For` was read left-most — the
  hop a client controls — so a spoofed header chose the rate-limit bucket. The
  right-most hop, the one the trusted proxy appended, is now used.

- Release signature verification no longer fails open. The project signs
  releases with keyless cosign and advertises them as cosign-verified, but two
  outcomes skipped verification and continued on a SHA-256 check against a
  checksum file fetched from the same origin as the archive — which whoever can
  serve the archive can also serve. Both paths now refuse unless the operator
  passes `--allow-unverified`. `enforce_cosign` is the single decision point, so
  a new outcome cannot quietly become a third way to skip. There is no
  pre-cosign release to tolerate; the cutover version was read off the releases
  rather than assumed.

### Fixed

- Sessions no longer die after a few tool-heavy turns. `AssistantToolCalls` and
  the `ToolResults` answering it are two separate history entries, but
  `trim_history` cut by message count. A cut landing between them left a tool
  result whose originating call was gone — a shape both OpenAI and Anthropic
  reject with a 400, so every later turn in the session failed until the user
  cleared it. `max_history_messages` defaults to 50 and each tool iteration
  appends two entries, so the boundary arrives in a handful of turns. A cut
  index is now moved forward — never back — until it is not a `ToolResults`,
  which keeps the caller's length cap intact.

- Tool calls are no longer extracted from unstructured prose for non-GLM models.
  The rule is stated directly above the parser, because content a model echoes
  from an email, a file or a web page could mimic a call. Three parts of the GLM
  fallback broke it. A line that was only a URL became a `shell` `curl`, so a
  model reporting what it had just read issued an outbound request from the
  operator's machine — under Full autonomy, or with `curl` allowlisted, with no
  prompt. `browser_open`, `browser` and `web_search` were aliased to `shell`
  despite being real registered tools.

- `Config::save()` writes the operator's config, not the environment's.
  `load_or_init` folds environment overrides onto the in-memory `Config` and
  `save()` serialised that same struct, so the first console, TUI or setup write
  baked whatever the environment held into `config.toml` permanently. A container
  started with `RANTAICLAW_ALLOW_PUBLIC_BIND=true` had a one-run exposure setting
  outlive its cause, and `RANTAICLAW_API_KEY` became a stored credential.

- `setup <topic> --non-interactive` fails loudly instead of exiting zero after
  doing nothing. A provisioner that errored, aborted or timed out printed a
  message and still returned `Ok`, and the caller saved the config regardless —
  while the abort message said "nothing saved". All three arms now return an
  error, which also makes "nothing saved" true rather than merely printed. It
  also answered every choice with option 0, which for MCP's multi-select picked
  a server the operator never asked for.

- A headless provider section no longer reports success for an install that
  cannot send. It treated a selection with no key as complete, which is
  reasonable interactively — the operator is sitting there and may export the
  key later — but headless it saved the config and exited zero for an install
  that cannot send a single message, with no way for an installer to tell.

- The gateway reads the config it is running. The config API and the
  hot-reloader both called `Config::load_or_init()`, which re-resolves the config
  path from the environment and the `active_workspace.toml` marker on every call.
  A marker or env var that changed after boot made a console write
  read-modify-save a *different* file and swap it into the running state, and
  made the reloader answer a change to the watched file by loading some other
  one. Both now load the path the gateway booted with.

- Synchronous gateway chat no longer prompts a TTY. A request could block on an
  approval prompt written to a terminal no operator was watching.

- The cron tools' object parameters have real schemas. An agent reported
  `cron_add` refusing its schedule because `every_ms` arrived as `"600000"`
  rather than `600000`; the underlying reason was that there was no schema for
  the field. `schedule` was a bare object whose shape lived only in a prose
  description, so a provider doing constrained or structured decoding had nothing
  to constrain against — the model was guessing in the absence of a type, not
  ignoring one. `schedule`, `delivery` and `cron_update.patch` now carry schemas
  derived from the types they deserialise into, in one module so `cron_add` and
  `cron_update` cannot advertise different shapes for the same field. A
  stringified integer is also accepted.

- The generative-UI render instruction is no longer stored as the user's words.
  The console appended it to the message body and the gateway persists
  `body.message` verbatim, so it became part of the user's turn, was replayed on
  every later turn — including after switching back to markdown — and appeared in
  exported transcripts as text they never wrote. Render mode is now a structured
  `render_mode` request field the gateway applies to the outgoing prompt and
  never persists. An absent or unrecognised value means markdown, so an older
  console keeps working.

### Changed

- One honest answer to "can this provider actually send?", shared by every
  surface that asks. `has_usable_credential` knew only about plain API keys, so
  `doctor` did not use it — it asked `resolve_key_for_provider`, which only looks
  at config, and told operators authenticated by environment or OAuth that the
  agent could not send while it sent fine. It now answers per auth mode, each
  branch calling the code that consumes the credential: AWS environment
  variables for Bedrock, auth profiles for Codex, the cached GitHub token for
  Copilot, the OAuth env vars and cache file for Qwen, the Gemini CLI, and
  providers that need no credential at all. `doctor`, the config API and headless
  setup all ask it, and the per-caller carve-outs are gone. `doctor` reports "no
  credential for X — tried config, its environment variables, and the provider's
  own auth" instead of "no API key", and no longer probes a provider
  unauthenticated when its credential is not a bearer it could send.

- Both the config API and headless setup resolve per-provider keys. They read
  the top-level `api_key` only, so a key stored under `provider_api_keys` — what
  the web console writes — read as absent.

### Documentation

- The release runbook no longer says a green `schema_drift` means a release
  rolls back cleanly. It means neither: the gate compares the working tree
  against committed snapshots, so it measures drift since the last PR that moved
  the schema, not since the last release — and the PR that bumps the schema
  commits the matching snapshot with it. 0.28.0-alpha is the worked example: all
  three release gates were green on the bump commit, and the release still
  carried config schema v26 → v27. The runbook now names the release-to-release
  check instead.

## [0.28.0-alpha] — 2026-09-04

Wave 0 of the production-readiness audit: the eight verified blockers, none of
which had reached a user before this release. Two are credential/authentication
fixes (a Telegram bot token reaching logs, a forgeable `Authentication-Results`
header granting owner authority over email), one is data loss (`ui install --dir`
deleting any directory that held a `.git`), and the rest cover the MCP transport,
the gateway's MCP lifetime, the TUI's crash-and-wreck-the-terminal path, and the
console's framework currency. **Config schema v26 → v27**: the release adds
`[channels_config.email] trusted_authserv_id` with a no-op migrate arm, so it
does **not** roll back cleanly to 0.27.1-alpha (a v27 config will not load on a
pre-v27 binary). claw-ui pin bumped to **v0.3.25**.

Two behaviour changes need an operator's attention, both narrowing:

- **Email owner recognition is off until `trusted_authserv_id` is set.** Mail
  from an `approval_owners` address is dropped rather than granted owner
  authority, and the channel says so at startup. Ordinary mail is unaffected.
- **A configured tunnel no longer authorises a public bind.** A config that binds
  `0.0.0.0` and relied on `[tunnel] provider` to make that acceptable now refuses
  to start; bind `127.0.0.1` (the tunnel reaches it there) or set
  `[gateway] allow_public_bind = true` explicitly.

No exposure-boundary default is widened.

### Fixed

- The gateway stops respawning every MCP server on every chat request. It
  builds an `Agent` per request, and agent construction discovered MCP tools —
  spawning each configured server, running the handshake and `tools/list`
  sequentially with a 30-second per-server timeout, then SIGKILLing the lot
  when the request's agent dropped. Every console turn paid that, threw the
  result away, and reset any server-side state; one hung server stalled every
  turn. MCP servers are now connected once and shared, with the pool owned by
  the gateway and rebuilt when a hot-reloaded config changes `mcp_servers`. A
  turn holds its pool for its whole lifetime, so a reload never pulls a client
  out from under an in-flight tool call. The TUI and CLI keep the existing
  one-agent-owns-its-servers shape.

- MCP servers survive being long-lived. Two transport defects only mattered
  once a server outlived a single request: its stderr was piped and never
  read, so a server that logged past the pipe buffer (~64 KiB — `npx` install
  progress is enough) blocked on its own `write` and stopped answering while
  looking healthy; and each caller read stdout itself until it saw its own id,
  binning every other reply, so a second concurrent call to one server waited
  out the full 30-second timeout. A background task now owns stdout and routes
  each reply to the caller waiting on its id, and a second task drains stderr
  (logged at DEBUG, length-capped). A server that exits mid-request now fails
  its caller on EOF instead of at the timeout.

- The TUI no longer panics on multibyte tool output. Two sites cropped text by
  byte index — the tool-status line in the transcript and the argument values
  in the calls overlay — so an `ls` over a filename with an accented or CJK
  character killed the render loop. Both now use the char-safe crop the repo
  already had for the same class of bug. The calls overlay was also counting
  bytes under a `chars` label.
- A panic in the TUI now restores the terminal before printing. There was no
  `panic::set_hook`, and the restore ran only when the loop returned `Err`, so
  a panic left raw mode, the alternate screen and mouse capture switched on —
  the message was invisible and the shell unusable until the user blindly
  typed `reset`.
- `ui install --dir <path>` no longer deletes a directory just because it
  holds a `.git`. "Managed" meant `server.js` **or** any `.git` directory, and
  a managed target both skipped the `--force` guard and was recursively
  removed before extraction — so pointing `--dir` at a dotfiles checkout or
  any clone destroyed it, with no prompt and no backup. A directory now counts
  as this installer's only when it holds `server.js`, the `.version` marker, or
  a `.git` whose remote is claw-ui's own (the pre-tarball installer used
  `git clone`, and that upgrade path still works). The `remove_dir_all` call
  re-checks ownership itself rather than trusting a guard 60 lines away.
- `CI Required Gate` — the one check branch protection is meant to require —
  now reads every job it runs. `channel-lark` was missing from its `needs:` and
  `docs-quality` was only checked on pushes, so both went red on a PR without
  blocking the merge; and a `skipped` result counted as a pass, so the
  `ci:full`-gated stages enforced nothing. The decision table moved to
  `scripts/ci/required_gate.sh` with a self-test the job runs first, every Rust
  stage now runs on every Rust PR, and the checks worth requiring
  (`workflow-sanity`, `sec-audit`) no longer carry `paths:` filters that would
  leave a required check permanently pending. (#698, #699, #700)

### Security

- The email owner gate no longer trusts a forged `Authentication-Results`
  header. That header is written by the receiving infrastructure, but anything
  a sender puts in a message arrives as a header too — and the gate read the
  first one it found without checking who wrote it. Combined with a spoofed
  `From:` matching an owner address, a sender obtained owner authority, which
  on this product is the full tool set, shell included. Two weaknesses
  compounded it: `dmarc=pass` was accepted regardless of which domain it named,
  and the domain comparison was a substring test, so
  `example.com.attacker.test` satisfied a check for `example.com`.

  **New config key** `[channels_config.email] trusted_authserv_id` names the
  authserv-id your own mail server writes (the first token of the header).
  Verdicts from any other verifier are ignored. **Schema version 26 → 27.**

  **Operator impact**: until this key is set, email owner recognition is off —
  mail from an `approval_owners` address is dropped rather than granted owner
  authority, and the channel says so at startup. Ordinary (non-owner) mail is
  unaffected.

- `ui install`/`ui update` now pin claw-ui v0.3.25, which bumps Next.js
  16.0.10 → 16.3.4. The console's login gate, CSRF rejection, expected-host
  allowlist and server-side gateway-token attachment all live in the
  proxy/middleware layer, and the pinned version sat inside the affected range
  of the published Middleware/Proxy bypass advisories. claw-ui CI also gained a
  runtime dependency audit so the version cannot silently rot back out of
  currency. (claw-ui #108)

- The gateway no longer accepts a configured tunnel as authorisation for a
  public bind. Every tunnel provider proxies `localhost:<port>`, so a tunnel
  never made a public bind necessary — but the guard let one through, and a
  tunnel that failed to start (bad token, missing binary) printed a warning
  and served the control plane on `0.0.0.0` with the operator having opted
  into nothing. Binding a public interface now requires
  `[gateway] allow_public_bind = true` and nothing else.

  **Breaking for one shape of deployment**: a config that binds `0.0.0.0` and
  relies on `[tunnel] provider` to make that acceptable will now refuse to
  start. Fix by binding `127.0.0.1` (the tunnel reaches it there) or by
  setting `allow_public_bind = true` explicitly.

## [0.27.1-alpha] — 2026-09-03

Console delivery release: rolls the claw-ui UI/UX wave out to `ui install`.

### Changed

- `ui install`/`ui update` now pin claw-ui v0.3.24: the console UI/UX wave
  (claw-ui #78–#107) — every panel on the verdict-band design system, chat
  turn-state honesty (failed tools surface in the activity header), one
  label language across forms, and small affordance/contrast fixes. (#688)

### Fixed

- CHANGELOG: the 0.26.0-alpha section header lost in a merge is restored. (#687)

## [0.27.0-alpha] — 2026-08-30

No baked-in default model. A fresh install no longer ships a hardcoded
`default_model`, so the TUI/console show the model **blank** until the operator
runs setup, and the agent **refuses to guess** a model rather than silently
running against one that then 401s or does not exist. **Config schema v25 → v26**
carries a no-op migrate arm (existing configs keep their explicit model; only
fresh defaults change), so it does not roll back cleanly to a pre-v26 binary.
claw-ui pin bumped to **v0.3.23**.

### Changed

- **`Config::default().default_model` is now empty.** Model resolution was
  scattered across the agent build, both agent-loop request paths, and the
  gateway/channels with three different hardcoded literals; all now fail fast
  with `no model is configured. Run \`rantaiclaw setup provider\` …` (a typed
  `agent::NoModelConfigured`). Existing on-disk configs (which carry an explicit
  `default_model`) are unaffected; `--model` / `RANTAICLAW_MODEL` overrides still
  resolve. `default_provider` is unchanged (still `openrouter`). (#683, #684)
- **`rantaiclaw status`** shows `(not set — run setup)`, and the console chat
  footer shows **"no model set"** instead of the word "default", when no model
  is configured. (#683, claw-ui #75 / v0.3.23)

### Fixed

- **`POST /api/v1/agent/chat` returns 400, not 500,** when no model is
  configured — the caller's to fix, with the actionable hint in the response
  body. (#685)

## [0.26.0-alpha] — 2026-08-28

Configuration and lifecycle hardening — the consolidated deep-scan of config
loading, the config API, setup/doctor, the daemon and service lifecycle, and the
console's configuration surfaces. **Config schema v23 → v25**: the release
carries migrations (`migrate_v24`/`migrate_v25`) that drop dead keys and align
serde defaults with the code, so it does **not** roll back cleanly to 0.25.0 (a
v25 config will not load on a pre-v25 binary). claw-ui pin bumped to **v0.3.22**.
No exposure-boundary default is widened.

### Security

- **`err_500` no longer leaks internal detail to the browser** — a failed config
  write returned the absolute `config.toml` path and the gateway `host:port`
  verbatim; the cause is now logged server-side and the response carries a
  stable, non-specific message. (#672)
- **Config-API secret redaction** — `api_url` credentials, proxy userinfo, and
  MCP `args`/`env` are redacted in the config response; a recursive JSON backstop
  covers every channel credential the typed redactor missed. (#642)
- **Doctor no longer leaks provider credentials** in its probe output. (#643)
- **MCP server API hardening** — loader env vars (`LD_PRELOAD`, …) can't be
  injected through a config write. (#644)
- **Config-API writes are validated before persisting**, so a console write can't
  brick the next startup. (#645)
- **`require_pairing` hot-reload**, **secret file-mode hardening**, and an
  **openclaw-migration TOML-safety** fix. (#646, #648, #652)
- **Config-change audit trail.** (#647)

### Added

- **Warn on unknown/mistyped top-level config keys** at load, with a
  nearest-match suggestion — a `[gatway]` typo no longer silently no-ops (K2). (#670)
- **Two advisory CI gates**: every CLI command must appear in the command
  reference, and every `*Config` field must have a runtime reader (generalized
  from the 15 channel structs to all 67). (#671)
- **A gate that checks every documented config default against
  `Config::default()`** — caught `default_model` drift on landing. (#681)

### Changed

- **The daemon exits non-zero on a fatal startup failure** (a refused public
  bind, an unparseable address, a persistent port conflict) instead of retrying
  forever behind a false "🧠 daemon started" banner; the banner is gated on the
  gateway's first successful bind, and channels now drain cleanly on shutdown
  instead of a bare `abort()`. (#673)
- **`doctor models` probes provider catalogs concurrently** — wall-clock drops
  from the sum of provider latencies to the max. (#680)
- **Setup and doctor honesty**: sections and checks report what they actually
  did; config env-override handling, default alignment, and migration robustness
  were tightened (schema v24/v25). (#660, #661, #662, #663, #664)
- **Dead config keys and the legacy-doctor path removed.** (#667, #674)
- **Docs corrections**: config paths resolve to the active profile dir, feature
  gating (`kb` is default, `hardware` is not), timeout defaults, and OOM build
  advice. (#670)

### Fixed

- **Config persistence** — safe persistence, atomic writes with a retained
  backup, and runtime-reload decrypt drift. (#650, #651, #653)
- **Lifecycle** — service-unit generation, gateway bind/shutdown, the daemon
  supervisor lifecycle and profile handoff, and auth lock + OAuth loopback. (#654,
  #655, #656, #657, #659)
- **Setup preserves unprompted config** instead of clobbering it. (#649)
- **Console (claw-ui v0.3.22)** — config-panel load/error states and stale
  snapshots, provider-secret clear/save resilience, and consistent typed API
  error handling. (claw-ui #72, #73, #74)

## [0.25.0-alpha] — 2026-08-26

Console chat surface hardening and features, consolidating a deep-scan of the web
console's Chat, Status, and Persona surfaces across the gateway and claw-ui.
**No config schema change** (still v23): the release adds no migration and rolls
back cleanly. claw-ui pin bumped to **v0.3.21**. The only exposure-boundary touch
is an advisory startup warning for an exposed-but-unauthenticated bind; nothing
widens a default.

### Security

- **Web-console tool approvals are scoped to the SSE turn that raised them.**
  Approvals were process-global and keyed by shell command basename, so one
  browser could see and resolve another turn's command, and two turns waiting on
  the same basename stalled until the auto-deny. Each turn now runs under a
  per-turn scope; requests carry a UUID (resolution is UUID-only, never
  basename), and a new `approval_resolved` SSE event tells the browser when a
  request is answered or expires so the modal can't be left with dead buttons.
  Per-session "Always" grants are keyed safely, revoked wherever autonomy is
  tightened, and cleared on session delete. (#640)
- **API rate limiting keys on the authenticated principal behind a proxy** — a
  bearer-token prefix rather than the peer IP, so every browser behind the BFF no
  longer shares one bucket. (#638)
- **Advisory warning when the gateway is reachable beyond localhost with no
  console login configured.** Print-only; it does not change the default bind. (#639)

### Added

- **Branch a conversation.** `POST /api/v1/sessions/{id}/fork` creates a child
  session (carrying `parent_session_id` and an origin system message) and leaves
  the parent open; surfaced as a Fork action on each console session row. (#639, claw-ui #71)
- **Transcript export** from the console (Markdown/JSON), formatted client-side
  from the stored transcript. (claw-ui #71)
- **Session usage tile** on the Status panel (sessions, messages, average, latest)
  from the existing insights endpoint. (claw-ui #71)
- **Persona editor** in the console: settable name, role, tone, avoid, and
  timezone, with presets served from `GET /api/v1/personality/presets`; edits sync
  the console header live. (#626, claw-ui #69)
- **Web-console pillar doc** and the gateway/console API-contract ADR. (#639)

### Fixed

- **Sessions & chat handlers hardened**: full-text search no longer 500s on a
  quote in the query, error responses no longer leak profile filesystem paths,
  temperature is clamped, session paging has a stable tiebreak, a deleted session
  is not resurrected mid-turn, a cancelled turn is not persisted, and the
  previously-dead `gateway.request_timeout_secs` is now wired to the timeout
  layer. Insights totals are aggregated in SQL (correct past 10k sessions). (#623)
- **Chat turn contract**: retrieved reference material is threaded as a structured
  field (kept out of the persisted transcript and replayed history), replay is
  capped, and a zero-token usage event is no longer emitted as real data. (#625)
- **Persona end-to-end**: preset resolution, validation, live channel reach, and
  removal of the dead SYSTEM.md write; the TUI persona command writes through to
  config. (#626)
- **Doctor and health honesty**: `/health` and `/readyz` are slimmed (no pid or
  last-error leak), doctor checks run without blocking the async runtime and
  report which checks were skipped. (#624)

### Changed

- Startup backfills session titles on a gateway-only deployment (previously only
  the TUI did). (#639)

[0.25.0-alpha]: https://github.com/RantAI-dev/RantAIClaw/releases/tag/v0.25.0-alpha

## [0.24.0-alpha] — 2026-08-21

Tools & Autonomy security hardening, consolidating a dedicated deep-scan of the
policy engine, the approval flow, the privileged tools, and the OS sandbox layer.
**No config schema change** (still v23): the release adds no migration and rolls
back cleanly. claw-ui pin unchanged at **v0.3.20**. Every change tightens or
honestly labels an existing surface; none widens an exposure boundary.

### Security

- **Command gate: quote-insertion no longer defeats the argument checks.** Shell
  arguments are de-quoted before the allowlist safety checks, so a value like
  `find . '-exec'` can no longer smuggle a dangerous flag past `is_args_safe` and
  reach `sh -c` as an executable token. Also floors git short-form abbreviations.
  (#607)
- **Rate limiter no longer self-disables on freshly-booted hosts.** The
  action-window computation underflowed when host uptime was under one hour,
  silently clearing the limit; the window is now preserved. (#604)
- **`forbidden_paths` has a non-removable floor.** System-critical paths
  (`/etc`, `/root`, `~/.ssh`, `~/.aws`, …) are always enforced even if config
  strips the list, and matching is case-folded. The config API no longer accepts
  a `forbidden_paths` override at all. (#610, #620)
- **`pty` and `ssh` no longer execute arbitrary commands ungated.** `pty start`
  and `ssh exec` now route through human approval (deny when no approver backend
  is present), and ssh file transfers are confined to allowed local paths. (#612)
- **Code-executing `git`/`cargo`/`npm` subcommands are classified Medium risk**
  so they prompt under the default policy instead of passing as low-risk. (#609)
- **`proxy_config` is owner-only and its credentials are redacted** in tool
  output. (#606)
- **Authority tools are gated under ReadOnly and refuse a wildcard owner.**
  `manage_permissions` and `issue_pairing_code` now hold a `SecurityPolicy`,
  refuse to grant to `*`, and are blocked at ReadOnly autonomy. (#608)
- **The shell approval flag can no longer self-clear the risk gate.** A
  model-set `approved` field is gone from the tool schema; an allowlisted but
  risk-gated command routes to a human, and a hard-blocked command is refused,
  not model-approvable. (#611)
- **The "Manual / Safest" preset forces every tool to prompt.** A `["*"]`
  wildcard in `always_ask` closes the fail-open where ~40 registered tools were
  auto-approved because the shared catalog only named nine (three of them
  phantom). `cron_remove` is now owner-only. (#613)
- **Session "Always" grants are revoked when autonomy is tightened,** so a grant
  made under a looser policy does not survive a lock-down. (#614)
- **Allow-command entries are validated to a single basename** (CLI and config
  API), rejecting multi-token or glob values that would silently never match, and
  warning on dangerous basenames. (#615)
- **Policy files are written atomically** (temp + rename) so a crash mid-write
  cannot truncate the autonomy/permissions state. (#619)

### Changed

- **An autonomy change now restarts channel listeners** and is part of the
  restart fingerprint, so a tightened policy reaches already-running channels.
  (#621)

### Fixed

- **CLI `status` and the `/autonomy` picker no longer misreport the preset.**
  `status` shows the enforced preset next to the level (so `Manual` and `Smart`,
  both `Supervised`, are distinguishable), and the TUI picker no longer claims
  success when a policy reload fails. (#616)

### Documentation

- **Sandbox/audit docs no longer claim a control that isn't wired.** `traits.rs`
  and module docs previously asserted a sandbox backend and audit logger were
  applied before every shell execution; the docs now state the real scope, and
  `is_path_allowed` / `read_command_allowlist` carry accurate scope comments.
  The `[security.sandbox]` layer, cost cap, and audit/resource limits remain
  configured-but-inert pending a wire-vs-delete decision. (#617, #620)

### Internal

- Policy test cleanup: the dead autonomy-cycle test now runs, the vacuous
  forbidden-path test is retargeted to a real assertion, and a dead fork-bomb
  string check is removed. (#618)

## [0.23.0-alpha] — 2026-08-20

Cron scheduler hardening and honesty pass, consolidating the cron deep-scan
effort. **Config schema v22 → v23** — a new `[cron] max_catchup_age_secs` field
is added on load. The migration is additive and preserves existing values, but a
config once opened by this release will not open in v0.22.x, so this is a
one-way upgrade. Bundles claw-ui **v0.3.20**.

### Added

- **`[cron] max_catchup_age_secs` (default `86400` = 1 day): a staleness gate on
  catch-up runs.** After downtime the scheduler no longer fires a job "late" — a
  due job whose `next_run` is older than the window is skipped instead of run: a
  recurring schedule re-anchors to its next future occurrence and stays enabled,
  a one-shot `at` job is disabled. `0` disables the gate. Firing that does happen
  is coalesced to a single run (missed occurrences are never replayed). (#600)

### Changed

- **A cron run blocked by the security policy is now recorded as `refused`, not
  `error`,** so an operator can tell a command that ran and failed apart from one
  the policy blocked before it ran. The TUI detail panel and the CLI `cron run`
  report render `refused` distinctly. (#599, #601)
- **Creating a shell cron job over HTTP whose command the fire-time gate would
  refuse now returns an advisory `warning`** ("created, but will not run on its
  schedule …") instead of silently succeeding; the job is still created
  unchanged, and the web console surfaces the warning as a toast. (#598, claw-ui
  #65)

### Fixed

- **Cron weekday field now follows standard crontab numbering.** The `cron`
  crate numbers weekdays the Quartz way (Sunday=1..Saturday=7); a 5-field
  crontab expression was passed through without remapping, so every
  weekday-constrained schedule fired one day early (`0 9 * * 1-5` ran Sun–Thu,
  not Mon–Fri) and crontab-Sunday (`0`) was rejected outright. The weekday field
  of 5-field expressions is now translated to the crate's numbering
  (`0`/`7`=Sunday, `1`=Monday … `6`=Saturday), matching standard crontab and the
  documented examples. **Behavior change:** existing weekday cron jobs shift by
  one day (toward the crontab-correct day) on their next reschedule; no stored
  data is migrated. 6- and 7-field crate-native expressions are unaffected.

### Security

- **`arrayref` pinned to `=0.3.9` (last known-good).** The crate — pulled
  transitively via `blake3` and `wa-rs-libsignal` — was compromised on
  2026-08-20: every legitimate version (0.3.5–0.3.9) was yanked the same day and
  a trojaned `0.3.10` was published that depends on `proc-macro1`, a
  proc-macro2 typosquat that pulls an HTTP client (`ureq`) to exfiltrate at
  compile time. The exact pin (plus a `deny.toml` allow-list for the
  yanked-but-immutable, safe 0.3.9) prevents any resolution from reaching the
  malicious version. Do **not** `cargo update -p arrayref` until arrayref
  publishes a verified-clean release. (#602)

## [0.22.3-alpha] — 2026-08-19

One TUI setup fix and one dependency advisory bump. No config schema change —
this release carries no migration and rolls back freely to v0.22.2-alpha.

### Fixed

- **TUI: the setup model list shows the full catalog, not the curated list
  alone.** After picking a provider in `/setup provider`, the model step
  offered only the ~10 curated entries while the same binary's `/model`
  picker, the gateway, and channel routing all served the cache-backed
  catalog (400 openrouter models on a refreshed box). The setup step now
  reads the same catalog, capped at 120 like the CLI wizard, with curated
  descriptions merged; a fresh install with no cache still sees the curated
  list. Also removes a drifted private copy of the per-provider default
  model table whose openrouter default named a model that no longer exists.
  (#569)

### Security

- **Dependencies: `h2` 0.4.13 → 0.4.16** for RUSTSEC-2026-0258 (unbounded
  queueing of empty HTTP/2 DATA frames — denial-of-service class, low
  severity; transitive via hyper/reqwest). (#570)

## [0.22.2-alpha] — 2026-08-18

Three TUI/config bugfixes. No config schema change — this release carries no
migration and rolls back freely to v0.22.1-alpha.

### Fixed

- **TUI: `/model` switches the agent's model, not just the status-bar label.**
  Both the typed form and the picker reported "Model set to: X" while only the
  label changed — every following turn silently ran (and billed) on the old
  model, and neither the config on disk nor the next launch learned about the
  switch. A model switch now persists `default_provider`/`default_model` and
  hot-reloads the running agent through the same path the wizard uses; a wrong
  model fails the next turn with the provider's own error instead of a false
  success. Model ids containing `:` (ollama `llama3:8b`) survive the split.
  (#565)
- **TUI: the setup provider list matches the CLI wizard again.** The TUI kept
  a hand-copied provider table that had drifted 11 providers behind — Kimi
  Code, Qwen Code, OpenAI Codex, Astrai, GLM (CN), MiniMax (CN), Qwen
  intl/US, Z.AI (CN), Synthetic, and OpenCode Zen were fully supported by the
  provider factory but never offered in the TUI. Both surfaces now read one
  shared table. (#566)
- **Config: one decrypt pass shared by startup and the TUI reload.** Startup
  (`load_or_init`) and the TUI's config reload each kept a hand-written list
  of which at-rest-encrypted secrets to decrypt, and the lists drifted:
  reloads rebuilt the agent with encrypted `provider_api_keys` blobs (every
  provider call then answered 401 until restart), and a reloaded Telegram
  channel polled with an encrypted token. Both callers now share a single
  decrypt authority, closing the per-provider key, Telegram bot token, and
  skill literal key gaps at once. (#565, #567)

## [0.22.1-alpha] — 2026-08-18

A single-hardening patch. No config schema change — this release carries no
migration and rolls back freely to v0.22.0-alpha.

### Added

- **Memory: the `memory_recall` tool follows the active conversation on
  interactive surfaces.** With a conversation set (TUI session, console API),
  the explicit recall tool reads through the layered path — the
  conversation's own rows first, the shared unscoped tier as backfill, other
  conversations' rows filtered — matching the scope the injection path
  already uses. Surfaces that serve many conversations through one registry
  (channels, the gateway webhook) deliberately keep the global read: a
  shared scope would race across concurrent turns, and guests cannot invoke
  the tool unless an operator widens `guest_allowed_tools`. (#562)

## [0.22.0-alpha] — 2026-08-17

No config schema change — this release carries no migration and rolls back
freely to v0.21.0-alpha. Memory *behaviour* changes substantially (that is the
point of the release): recall is more selective and conversation echoes never
reach prompts.

### Fixed

- **Memory: auto-saved conversation turns are never injected into prompts.**
  Reproduced live twice: a TUI request for a story was answered with an old
  Telegram request's Python tutorial, and a bare "hello" recalled four stale
  turns and executed tools (`skills_list`, `glob_search`) nobody asked for.
  Every auto-save path stores under the `conversation` category; the context
  builder now excludes that category outright — rows stay stored, reachable
  via `memory_recall`, and pruned by retention. Covers legacy rows on every
  backend with no migration. (#555)
- **TUI: each session's memory reads and writes are scoped to the
  conversation.** The interactive agent ran unscoped: it recalled every
  channel's rows and its auto-saves landed in the shared tier where every
  surface could recall them. Each submit now carries `tui:<session_id>`
  (the same `ConversationKey` format channels use), applied per turn like
  the gateway. (#556)
- **Memory: relevance scores are absolute, so the floor can reject a weak
  set.** Scores used to be rescaled relative to the best hit — the top hit
  always scored 1.0 and something was injected on nearly every turn. Scores
  are now absolute in [0, 1]: cosine for the vector signal, query coverage
  for the keyword paths (BM25 still orders hits; its corpus-dependent
  magnitude — measured near 1e-6 on a small store — cannot be the score),
  match-tier fraction on postgres. A query with no relevant memory injects
  nothing. Keyword-only recall (`embedding_provider = "none"`) is exact-term
  matching; configure an embedding provider for semantic recall. (#558)
- **CI: the weekly Sec Audit cron can file its advisory issue** instead of
  failing with "Resource not accessible by integration" on a zero-vuln
  scan. (#557)

### Added

- **Agent: the interactive system prompt nudges the model to save durable
  facts** (preference, standing decision, project fact, correction) via
  `memory_store` with category `core` and a stable key, the moment the user
  states them — complementing the existing pre-compaction flush. Gated to
  interactive surfaces (channel prompts serve guests, whose words are never
  nudged into durable memory) and to registries that actually carry
  `memory_store`. (#559)

## [0.21.0-alpha] — 2026-08-16

No config schema change — this release carries no migration and rolls back
freely to v0.20.1-alpha.

### Added

- **TUI: drag-select chat text and copy it with Ctrl+C.** Native terminal
  selection died silently in v0.10.0-alpha when mouse capture was enabled for
  wheel scrolling (capture is all-or-nothing — the protocol has no
  "wheel-only" mode). Selection now lives in the app: drag highlights whole
  transcript lines, Ctrl+C with a highlight copies via OSC 52, Ctrl+C without
  one keeps its cancel/quit meaning, Esc clears the highlight first and
  cancels the turn second, a plain click deselects. The copied text is the
  message text itself — no `│` borders, no wrap-injected line breaks, code
  fences verbatim — which is better than the pre-v0.10 native copy ever was.
  Honest limits: OSC 52 delivery can't be confirmed from inside the app and
  GNOME Terminal/VTE don't implement it — the status notice says so, and
  Shift+drag native selection remains the universal fallback. Wheel
  scrolling is untouched.

### Changed

- Web console pin rolls to **claw-ui v0.3.19**; the `unexpected_host`
  console lockout is now documented in the troubleshooting guide.

## [0.20.1-alpha] — 2026-08-16

A single-fix patch. No config schema change — this release carries no
migration and rolls back freely to v0.20.0-alpha.

### Fixed

- **Setup no longer saves a provider it cannot start, and a broken provider
  config no longer locks you out.** Choosing a key-required provider (openai,
  anthropic, gemini without CLI auth) and leaving the API key empty used to
  save silently — and every later launch, including `rantaiclaw setup`, then
  died with `openai: OPENAI_API_KEY required` before any UI existed; the only
  escapes were hand-editing `config.toml` or exporting the env var. Two
  halves: setup now checks whether the chosen provider can actually construct
  keyless (the same check boot performs, so an exported `OPENAI_API_KEY` or
  gemini CLI auth still sails through) and offers re-enter/abort instead of
  saving a broken pair; and the TUI now boots without an agent when the
  provider fails to build — it reports what broke, opens provider setup, and
  the session heals in place once a working config is saved, no restart
  needed. Keyless-capable providers (ollama, openrouter, …) are untouched.

## [0.20.0-alpha] — 2026-08-16

The channels release: a full-subsystem deepscan (plans 115–149) plus
twenty-six follow-ups, covering all sixteen transports. Sixteen security
fixes, forty-one behaviour fixes, six additions, five changes. Every guard
mutation-tested; the TUI, the web console and the Telegram runtime driven
live before this cut.

Minor, not patch: the config schema moves **v18 → v22** and the gateway's
Telegram replies gain a field. **The schema migration is one-way** — once any
command of this release touches `config.toml`, older binaries refuse to start,
and the migrator writes no backup. Take your own copy of `config.toml` first
if you may want to roll back.

Console pin moves to claw-ui **v0.3.18** (Shift+Tab no longer changes autonomy
from a button, honest channel status, a reload banner that matches what the
gateway actually did).

### Operators: read before upgrading

- **Back up `config.toml` before first run.** v18 → v22 is one-way and
  unbacked. This is the single most likely thing to cost you time: a binary
  from v0.19.0-alpha or earlier will refuse a config this release has touched,
  and there is no downgrade path.
- **Six channel allowlists that defaulted to "allow anyone" no longer do.** If
  you configured Discord, Slack, Mattermost, Signal, IRC or QQ through setup
  and never typed an allowlist, that channel answered anyone who messaged it.
  It now denies by default — **re-run setup and enter your ids**, or the
  channel will stop responding to you too.
- **`[channels_config.webhook].port` was removed (schema v21).** Nothing read
  it; the migration drops it silently. No action needed unless you scripted
  against it.
- **Inbound images are budgeted at 20 per sender per 10 minutes.** A sender
  over budget gets a note instead of an image. Not configurable in this
  release, by design.
- **Telegram now reports the numeric user id as the sender, not `@username`.**
  If you keyed anything off the old value — an allowlist entry, a memory scope,
  an external script — it will not match. Numeric ids in `allowed_users` have
  always worked and are unaffected.
- **IRC resolves identity from the services account, not the nick.** On a
  network without account-tag capability, owner authority cannot be granted;
  the channel says so rather than trusting a nick anyone can take.
- **Replies now thread on Discord, Telegram and Mattermost.** Set
  `[channels_config] thread_replies = false` if you preferred flat replies.

### Security

- **A shell approval raised from a chat can now be answered with a bare `ok`
  there — and only there.** `ShellTool` is a `Tool`, and the trait carries no
  originating message, so its approvals registered with no chat attached.
  Unscoped requests are deliberately not resolvable by a bare `ok`/`y`, so the
  only way to answer one from a chat was to name the command (`allow brew`)
  while every other approval took a bare yes. The turn's chat now reaches the
  tool, so a shell approval names where it came from.

  This **widens** what a bare verb can resolve, deliberately. Three things bound
  it: a bare `ok` from a *different* chat is still refused, the owner gate is
  unchanged, and resolution still refuses when more than one pending request
  matches — so two parallel `curl` calls cannot be answered by a guess. Direct
  TUI and CLI runs have no chat and stay unscoped exactly as before.

- **Shift+Tab can no longer change the autonomy level from inside an approval
  prompt.** The approval handler ignores modified keys and Shift+Tab carries
  Shift, so the binding fired while a gate was on screen — and one of its rungs
  is "no prompts". It is now inert while an approval is pending or a turn is in
  flight, the cycle skips "off" entirely (use `/autonomy off`), an accidental
  press no longer force-rewrites hand-edited policy files, and a failed reload
  leaves the previous level in place instead of announcing one that is not in
  force.

- **Approving a tool call no longer widens the allowlist for a call it did not
  resolve.** The grant ran before resolution, and resolution matched by command
  basename — which fails when two calls share one. Since the agent runs tool
  calls in parallel, two pending `curl` calls were ordinary: pressing `A`
  permanently allowlisted `curl`, resolved neither call, and hung the turn
  behind a message claiming the request was no longer pending. It resolves by
  request id now, and grants only after that succeeds.

- **`/pair` no longer writes the pairing code into `sessions.db`.** The store is
  full-text indexed and long outlives the code's window. The code is shown on
  screen and recorded redacted. The channel name is validated (a typo used to
  mint a code under a surface nothing reads), owner-granting is now opt-in
  (`--owner`), and codes are single-use. **Treat any code previously minted
  through `/pair` as recorded and supersede it.**

- **`autonomous_tools` is visible.** The flag that skips the approval gate
  entirely appeared on no TUI or CLI surface, so `permissions show` could read
  "Owners (none)" while every channel message ran tools unprompted. It now heads
  `permissions show`, and `/channels` carries an approval-boundary row.

- **The public channel webhooks verify the bytes they parse.** Linq and
  Nextcloud Talk authenticated a `from_utf8_lossy` copy of the request while
  acting on the raw body — every invalid sequence collapses to `U+FFFD`, so the
  string that was verified was a many-to-one projection of the body that was
  processed. The WhatsApp handler already did this correctly.

- **WhatsApp, Linq and Nextcloud Talk webhooks are rate-limited and
  deduplicated.** Neither existed on any of the three, while `/webhook` in the
  same file has had both since it was written. A redelivery — which the
  platforms perform when an ACK is slow, no attacker required — ran the full
  LLM turn again, and a captured signed POST was replayable indefinitely.
  Nextcloud Talk now also records the nonce it was already given, and each
  handler acknowledges before running the turn so a slow turn no longer causes
  the retry in the first place.

- **WhatsApp Web stopped logging message bodies — including pairing codes.**
  Every inbound body was logged at INFO, and the `/claim` codes that promote
  their holder to owner were logged *before* the pairing handler ran. The
  surviving line is DEBUG and carries a character count. **Expire any
  outstanding pairing codes** (`rantaiclaw channels pair whatsapp` mints fresh
  ones) rather than trusting log cleanup — codes already emitted are burned.

- **The WhatsApp Web pair code and QR are no longer written to a non-TTY.** A
  managed daemon's stderr is captured by the journal, so linking credentials
  were landing in a log nobody chose to hold. They render only for an
  interactive terminal now.

- **The WhatsApp Web allowlist now applies to replies.** It ran only for
  non-JID recipients, and the reply target is always a JID — so every
  agent-driven send bypassed it entirely and the allowlist provided zero
  outbound containment. Groups and broadcasts stay exempt, deliberately and
  documented. A blocked send is now an error instead of a silent success the
  agent recorded as delivered.

- **An unmapped WhatsApp LID no longer passes on a non-empty allowlist.** The
  check read `has "*" || !is_empty()`, so configuring *any* entry admitted every
  unmapped-LID sender. It now needs an explicit `*` or an explicit `lid:<id>`
  entry, and such a sender is reported as `lid:<id>` so it can never be mistaken
  for a phone number in logs or in `approval_owners`.

- **The WhatsApp Web session database is 0600 (parent 0700).** It holds the
  account's long-term Signal keys and was created at the process umask, while
  every other credential store in this repo is 0600. **If the file was
  previously world-readable, re-link the device.**

- **WhatsApp Web pairing refuses to run over an unreadable session.** A corrupt
  session DB looked identical to "no session", so the wizard paired a fresh
  device over existing key material.

- **Telegram reports the numeric user id as the sender, not the `@username`.**
  A Telegram handle can be released and re-registered, and pairing writes
  whichever form the channel reports into `approval_owners` — so whoever took a
  released handle inherited owner authority. The username is kept as an alias,
  so an owner already recorded by handle is still recognised, but the primary
  form is now the one that cannot be transferred.

  **This changes the conversation-scope key, so existing Telegram threads start
  a fresh history.** Nothing is deleted; the old history is keyed under the
  username and simply no longer matched.

  `rantaiclaw permissions show` now flags any non-numeric owner entry while
  Telegram is configured, so you can see which of your entries are transferable
  and re-add them by id.

- **IRC resolves identity from the services account, not the nick.** A nick is
  a first-come lease: anyone who connects while the owner is offline — or
  forces them off with a ghost or a netsplit — takes it, and was resolved as
  that owner with the full toolset and authority to approve shell commands. The
  client now negotiates `account-tag`/`extended-join` (via a proper `CAP LS`
  intersection, so a server missing one capability no longer loses the others),
  uses the account as the sender, and **refuses a message from a nick listed in
  `approval_owners` that carries no account tag**. On a network without
  services that means owner authority over IRC is unavailable — deliberately.
  `approval_owners` for IRC now holds services account names. A literal `*`
  there still means "the operator turned the owner gate off".

- **IRC refuses to send a password over an unverified TLS link.** SASL PLAIN is
  reversible base64 and NickServ IDENTIFY is plaintext, so `verify_tls = false`
  plus any configured password handed the credential to whoever answered the
  connection. That combination now fails at startup unless the new
  `[channels.irc] allow_insecure_tls_with_password` is set, and disabling
  verification always logs a warning naming the server. **Rotate any IRC
  credential already used over such a link.**

  **Config schema v19 → v20.** Additive; the refusal is the default, and the
  migration deliberately does not grant the opt-in on an operator's behalf.

### Fixed

- **A WhatsApp Web message the agent was too busy to take now says so.** When
  the dispatch queue is saturated the inbound message is dropped rather than
  parking the wa-rs protocol loop — the right trade, but it was silent to the
  sender: no reply, no reason, and nothing to distinguish a busy agent from a
  broken bot. The chat is now told, at most once per minute per chat so a burst
  produces one apology rather than one per message. The notice goes out through
  WhatsApp directly, not the agent queue, and is not sent when the runtime is
  shutting down — there is nothing to try again with.

- **`NO_PROXY` is published before the proxies it exempts from.** The four
  proxy variables were written to the process environment in declaration
  order, so for the length of that loop a subprocess could read a proxy with
  no exemption list beside it. The ordering is data now — one list, `NO_PROXY`
  first.

- **WhatsApp Web traffic now honours `[proxy]`.** Its HTTP transport
  (`wa-rs-ureq-http`) built a client with no proxy configuration at all, so an
  operator who routed the agent through a proxy had this one channel quietly
  going direct — including its media downloads. It is replaced by a
  reqwest-backed transport that takes its proxies from the same configuration
  every other channel uses. `ureq` and `ureq-proto` leave the dependency graph
  as a side effect, not as the point.

- **A memory recalled from a private chat no longer surfaces in a group.** The
  layered-memory scope was built from the *sender* while conversation history
  was keyed on the *chat*, so one person's DM, every group the bot shared with
  them, and every forum topic collapsed into a single memory scope — a detail
  stored in private could be recalled into a public room by the same person's
  next message there. Memory now uses the same key as history, by calling the
  same function, so the two cannot drift apart again. Plan 118 fixed this for
  history and recorded that memory still had it.

  **Effect on existing installs**: memory stored under the old sender-keyed
  scope is no longer recalled under the new chat-keyed one. Nothing is deleted,
  and shared/global memory still backfills as before.

- **The daemon's health surface stops reporting a dead channel as healthy.**
  The supervisor's 30-second heartbeat marked every running channel OK
  unconditionally — it never asked the channel anything. `health_check()` was
  implemented with a real network probe by sixteen channels and had exactly one
  caller: the one-shot `doctor channels` command. So an expired bot token, a
  revoked webhook or a workspace the bot had been removed from left the
  listener task alive and the status green, while the channel answered nothing.
  The heartbeat now runs the probe. Two bounds keep that safe: a 10-second
  timeout, so a platform that accepts the connection and never answers cannot
  freeze the status, and **three consecutive failures** before the channel is
  reported unhealthy, so one dropped packet does not flap it. The probe runs in
  its own task, so a slow platform cannot stall message delivery.

- **An image sent to a model that cannot see images no longer breaks the
  conversation.** The vision gate counted image markers across the **whole**
  history, so once a stored turn carried a picture every later message failed
  with `provider_capability_error` too — the chat stayed dead until someone
  cleared its history out of band. Historic images are now replaced with an
  explicit `[image omitted: this model cannot receive images]` note before the
  gate runs; only the turn just sent can still be refused, and its message now
  says the conversation is not stuck.

- **Telegram photos obey the operator's `[multimodal]` limits.** That path
  carried its own 25 MiB constant, checked only `Content-Length` (advisory),
  never sniffed the bytes, and the caller dropped every failure with
  `if let Ok(..)` — a photo that could not be fetched vanished with no reason
  given. It now goes through the shared media policy like Discord, WhatsApp
  Cloud and Linq, and a rejection reaches the user as a note.

- **Configuring a channel in the TUI now starts it.** `/setup channels` wrote
  the config, printed "✓ configured", and left the live runtime untouched for
  the rest of the session — the change detector compared against a config that
  had already been swapped in, so it never saw a change. It now compares before
  the swap, by content rather than by count (rotating a leaked token is
  count-neutral and used to leave the listener polling with the old
  credential), and restarts as soon as the save completes instead of waiting
  for you to press Esc.

- **QQ appears in `/channels`.** It was missing from both of the TUI's private
  channel lists, so configuring it left the count at zero and it showed up in
  neither the configured nor the not-configured section. Both lists are gone;
  the TUI reads the same 16-entry catalog the rest of the runtime does, which
  also fixes Matrix and Lark being offered by setup but never displayed.

- **`/channels` stops claiming a dead runtime is polling.** The status panel
  read the runtime state three times while building one panel (so its rows could
  contradict each other), two teardown paths left the state at "starting" —
  which renders as "running" — and a panic in the channels task left it there
  forever. Per-channel rows are also labelled "runtime …" now, because the state
  they report is process-wide and never was per-channel.

- **Image links no longer vanish from outbound messages.** The renderer's AST
  builder matched neither the image tag nor its close, so the URL was discarded
  on all eighteen channels: an image written in markdown arrived as the bare
  word `chart`, and an alt-less image produced an empty paragraph. Each target
  now spells it its own way — a link on Telegram and Matrix (neither renders an
  inline image from markdown), `![alt](url)` on markdown targets, `alt (url)` on
  the flat ones.

- **A turn with nothing to say no longer records a delivery failure.** The
  splitter guarantees at least one chunk and returns an empty string when there
  is nothing to emit; the send paths posted it, Discord answered "cannot send an
  empty message", and the dispatch loop logged a failed delivery. Reachable from
  whitespace-only content, an image-only paragraph, and a reply that was
  entirely a tool-call block.

- **WhatsApp Web survives a restart.** Every listener restart leaked a live
  client, a sync worker and a device-saver onto the same SQLite session file —
  N restarts, N concurrent writers on one Signal store. The listener now aborts
  and awaits any previous handle first, drops the stray `ctrl_c` arm that
  reported a clean exit independently of the shutdown token, classifies terminal
  events instead of swallowing them in `_ => {}`, and clears the client so
  `health_check` can report false on a dead session.

- **WhatsApp Web pairing terminates.** The declared `timeout` was never read, so
  `PairEvent::Timeout` had exactly one occurrence in the repo — the arm that
  handles it.

- **WhatsApp Web snapshots are readable.** `snapshot_db` copied a WAL database
  without its sidecar, which is what produced the malformed key blobs the
  connect path then panicked on. It uses `VACUUM INTO` now, and a malformed blob
  is an error rather than a panic inside the SQLite row callback.

- **The WhatsApp Web mutation-MAC cache round trips.** `value_mac` was stored
  JSON-encoded and read back raw. The table is derived state, so it is cleared
  once on upgrade and the app-state sync repopulates it.

- **Long replies are no longer lost on Slack, Mattermost, WhatsApp and
  Nextcloud Talk.** Only three of eighteen channels split; the rest posted the
  whole rendered reply in one request, so any answer past the platform cap
  failed the *entire* send and you got nothing at all. Four more now split, each
  against a documented limit (Slack 4000, Mattermost 16383, WhatsApp 4096,
  Nextcloud Talk 32000). The others are deliberately left unsplit — no
  authoritative limit was found, and a wrong constant fails sends in a way that
  looks like an outage.

- **WhatsApp, Linq, Lark and DingTalk carry the platform message id** instead of
  minting a UUID per inbound message, so a redelivery is detectable rather than
  running the agent again on a message it already answered.

- **The Slack health probe can fail for a revoked token.** Slack answers
  `auth.test` with HTTP 200 and `{"ok": false}`, so a status-only probe reported
  healthy for exactly the condition it existed to catch.

- **The Linq chat id is percent-encoded** in the three URLs that interpolated it
  raw. It arrives on the inbound webhook and the request carries a bearer token.

- **A failed profile-root lookup is logged on every channel.** The helper was
  copy-pasted into fifteen files and the iMessage copy had dropped its error log,
  so a profile-resolution failure there presented as "no pairing code matched".

- **DingTalk stopped reconnect-storming.** A WebSocket *error* was reported to
  the supervisor as a clean exit, and the clean-exit arm marks a health error
  *and* resets the backoff — one event read as both failure and success. An
  expired ticket or a rate limit therefore reconnected every two seconds
  forever, never escalating toward the 60-second cap, burning the exact API
  budget backing off would protect. A transport fault now returns `Err`; a
  server-side `Close` stays a clean exit.

- **Signal's backoff resets on a working stream, not on a bare connect.** It
  reset the moment the HTTP response was 2xx, before a single event was read,
  and then slept a literal two seconds after the stream ended rather than using
  the backoff at all.

- **Signal no longer drops non-Latin messages.** Any SSE chunk that split a
  multi-byte character failed to decode and was discarded whole. The stream is
  now buffered as bytes and split on line boundaries, so a partial character
  simply waits for the rest of itself.

- **Signal refuses an unroutable reply target.** Anything that was not an E.164
  number or a UUID was treated as a group id, so a typo or a truncated
  identifier was sent to signal-cli as a group. `group:` is now the only route
  to a group, and `send` reports a clear error otherwise.

- **QQ's dedup evicts the oldest id, not an arbitrary one.** Eviction walked an
  unordered `HashSet` under a comment claiming it dropped the oldest, so a
  just-inserted id could be discarded — and a dedup miss costs a complete extra
  LLM turn plus a duplicate reply.

- **DingTalk ACKs a frame before filtering it.** Empty content, a consumed
  pairing code and an unpaired sender all skipped the ACK, so DingTalk
  redelivered frames from exactly the population the pairing flow exists to
  serve.

- **DingTalk's session-webhook map is bounded and validated.** It stored two
  entries per message forever and ignored the expiry DingTalk ships alongside
  them. Entries now carry that expiry and are swept; a URL that is not an HTTPS
  DingTalk endpoint is refused rather than stored, and the read lock is no
  longer held across the outbound POST.

- **Pairing grants access immediately on DingTalk, QQ, Nextcloud Talk and
  Matrix.** Their allowlists were plain `Vec<String>`, so a successful
  `/bind`/`/claim` was persisted but did nothing until the daemon restarted.
  They now also honour a console or CLI allowlist edit at runtime.

- **Telegram typing indicators no longer fight between chats.** One typing slot
  was shared by the whole channel, so starting typing for chat B silently killed
  chat A's indicator — and with the runtime's parallel message path, concurrent
  chats are the normal case. Handles are now keyed by recipient. The channel's
  own 4-second refresh loop is gone as well: the runtime already calls
  `start_typing` on a 4-second cadence, so every tick aborted the task spawned
  four seconds earlier and spawned another.

- **Telegram streaming drafts are no longer truncated for CJK and emoji.** The
  draft was gated and cut in *bytes* against a *character* limit, so a
  non-Latin reply was cut at roughly a third of its intended length. The
  neighbouring `finalize_draft` had this right already.

- **IRC replies stopped vanishing after a disconnect.** The write half survived
  the listener, so `send()` wrote into a half-closed socket and returned
  `Ok(())` with the reply lost and no error anywhere. Every exit from the
  session now clears it, and `send()` reports "IRC not connected" instead.

- **IRC no longer floods itself off the network.** Reply chunks went out
  back-to-back, which most servers disconnect as excess flood — so a long reply
  failed more reliably than a short one. Lines are now paced after a short
  burst. The `433` nickname-in-use retry is capped as well, instead of
  producing an unbounded NICK flood against a server that rejects every
  candidate.

- **The IRC health check no longer opens a connection per heartbeat**, which was
  the single most reliable way to get the bot K-lined, and said nothing about
  whether the live session worked. It now reports on that session.

- **IRC allowlist edits reach the running channel** (`apply_allowed_senders`).

- **Email now requires an authenticated sender before it honours `From:`.**
  The header was taken verbatim as the identity — no SPF, DKIM, DMARC or
  `Authentication-Results` check existed anywhere in the channel — so anyone
  who could deliver mail into the configured mailbox chose who the agent
  thought was talking, including an address listed in `approval_owners`.
  `Authentication-Results` is now parsed, and `dmarc=pass` or an aligned
  `spf=pass`/`dkim=pass` is required. Mail claiming an owner address is
  refused when it did not authenticate, **regardless of configuration**; the
  new `[channels.email] require_authenticated_sender` (default `false`)
  extends the same requirement to plain chat. Refused mail is dropped with a
  warning, never renamed to the shared identity `"unknown"`.

  **Config schema v18 → v19.** Additive with a serde default, and the default
  is the previous behaviour, so existing mailboxes keep working; only the owner
  path tightens unconditionally.

- **Email no longer hands SMTP credentials to a plaintext transport.** With
  `smtp_tls = false` the password went out in the clear with no warning. The
  transport now branches three ways and refuses credentials over plaintext; a
  credential-less local relay still builds.

- **The mailbox password no longer renders in `Debug` output.** `EmailConfig`
  derived `Debug` over the plaintext credential, so one `debug!(?config)` wrote
  it to the log stream. **Rotate any mailbox password that may already sit in
  retained logs** — the fix stops new leaks, it cannot recall old ones.

- **Email stopped losing mail and stopped refetching it forever.** Unparseable
  messages were filed `\Seen` alongside good ones and vanished; a batch where
  *every* message failed to parse flagged nothing at all, so the next poll
  returned the same UIDs indefinitely. Unparseable UIDs now get `\Flagged` as
  well, leaving UNSEEN while staying visible for review.

- **Email timestamps were off by up to ±14 hours.** The date was rebuilt into a
  `NaiveDate` and read as UTC, discarding the header's offset.

- **`<script>` and `<style>` bodies no longer reach the agent's prompt**, and
  HTML entities in mail bodies are decoded rather than passed through raw.

### Added

- **Linq images are fetched under the media policy instead of handed to the
  agent as a URL.** The channel emitted `[IMAGE:<the platform's URL>]`, which
  meant the image either silently never loaded (remote fetch is off by default)
  or was fetched with no size cap and its type taken from the payload — the
  exact combination `docs/security/inbound-media-policy.md` exists to prevent.
  It now goes through the same bounded fetch, byte sniffing and visible
  rejection as Discord and WhatsApp Cloud.

- **Discord and WhatsApp Cloud accept inbound images.** A screenshot sent to the
  bot on either platform was dropped without acknowledgement; the agent could
  already reason about images, it just could not receive them anywhere people
  send them. Both channels now fetch the attachment, sniff its real type from
  the bytes, and hand it to the multimodal path as a `data:` URI. Rejections —
  too large, wrong type, fetch failed — appear as a note in the message instead
  of silence. The rules are written down once in
  `docs/security/inbound-media-policy.md`, not per channel.

- **Email accepts inbound images.** An image attached to an email never reached
  the agent. The only attachment handling sat in a fallback branch that runs
  when a message has *neither* a text nor an HTML body — so a screenshot
  attached to an ordinary email, which is nearly all of them, was dropped
  without a word. Attachments now go through the same policy as every other
  channel, and because the IMAP message already carries the decoded bytes there
  is no fetch, no credential and no attacker-chosen host involved. Parts that
  neither claim to be an image nor look like one — calendar invites, vCards,
  delivery reports — are left alone rather than annotated, since an email's
  attachment list carries protocol furniture a chat platform's does not.
  Anything that is, or claims to be, an image still always produces a marker.
  `require_authenticated_sender` is unchanged: attachments are read only after
  the sender has been accepted.

- **Inbound images are budgeted per sender: 20 images per 10 minutes.** The
  media policy named this as its own known gap — inbound media was an unmetered
  cost lever for anyone the allowlist admits, and on a group channel that is a
  wider set than the operator pictures. The count is kept per
  channel-qualified sender (`discord:<id>`, `email:<address>`), so one
  identifier reused on two platforms does not share an allowance, and it is
  charged **before the download**, so an exhausted sender costs no bandwidth.
  Past the budget an attachment becomes a note naming the wait, like every other
  rejection. Both numbers are constants in `src/channels/media.rs`, not config
  keys — there is no schema change and nothing to set. Telegram and WhatsApp
  Cloud each make one authenticated lookup before the fetch; both check the
  budget before that call too, so an exhausted sender cannot make it either.

- **The gateway reports whether a channel save restarts the runtime.** The
  Telegram connect/allowlist and disconnect replies gain `restarts_runtime`,
  the same decision the human-readable `note` already carried. A console
  cannot branch on prose, so it guessed — and the web console's own hint said
  every save reloads the runtime while this reply said it does not, both on
  screen about the same click. The note is now derived from the flag, so the
  two cannot drift.

- **Replies thread on Discord, Telegram and Mattermost.** The threading seam
  existed and was plumbed through every dispatch site, but exactly one channel
  (Slack) filled it — so in every busy group the bot's replies landed flat.
  Discord now anchors on the prompting message (`message_reference`), Telegram
  on `reply_parameters` for text sends, and Mattermost moved off packing
  `"channel_id:root_id"` into the recipient onto the typed field, with its
  observable behaviour unchanged. `[channels_config] thread_replies` (default
  `true`, schema v22) turns it off without turning off the channel;
  `[channels_config.mattermost] thread_replies` still overrides it. The
  remaining nine channels are designed, not built — see
  `docs/project/2026-08-14-threading-design.md`.

### Changed

- **Setup no longer collects two credentials that nothing uses.** Slack's
  `app_token` (for a Socket Mode path this build does not implement) and Lark's
  `encrypt_key` (which the channel now refuses to start with, since it cannot
  decrypt event bodies) were both prompted as secrets. Neither prompt remains.
  A Slack config that still carries `app_token` logs a startup warning saying it
  is ignored, rather than accepting it in silence.

- **`[channels_config.webhook].port` was removed (schema v21).** Nothing read
  it: the webhook arrives on the gateway's own listener (`[gateway].port`), so
  the key told operators to open a firewall port nothing binds and then the
  callback silently never arrived. An existing config that still carries it
  loads unchanged — serde ignores the key — so there is nothing to do. The setup
  wizard no longer asks for it.

- **Setup no longer defaults a channel allowlist to "allow anyone".** Six
  provisioners pre-filled the allowlist prompt with `*`, and three of them also
  mapped an *empty* answer to `*` — under a prompt whose own label read
  "empty = deny all". Pressing Enter through setup therefore opened the channel
  to every sender on the platform. The prompts now start empty, an empty answer
  stays empty, and both an empty list and a `*` list produce an explicit
  warning. Typing `*` still works and is still honoured.

  This tightens an exposure surface rather than widening one, so no config
  schema change or version bump is involved — the affected defaults are setup
  prompts, not `config.toml` values. Channels already configured are untouched;
  this only changes what a *new* setup run writes. If you relied on pressing
  Enter to get an open channel, type `*` at the prompt instead.

- **Setup refuses to save a credential the platform rejected.** Every probing
  provisioner used to warn and persist anyway, so a typo'd, expired or revoked
  token was written to `config.toml` with the same "configured" state as a
  working one. A 401/403 now stops the write unless you explicitly confirm.
  A *transport* failure (DNS, timeout, offline) is treated as inconclusive and
  still defaults to saving, so air-gapped and offline installs keep working.

- **A provisioner that stops early no longer counts as success.** Emitting a
  failure and then returning `Ok(())` made both setup drivers install the core
  skill and save the config, producing a false "channel is set up" signal.

## [0.19.0-alpha] — 2026-08-11

The Knowledge Base release: 29 fixes and features from a full-subsystem audit
(plans 086–114), every guard mutation-tested, every surface driven live (TUI
via tmux, web console via browser automation) before this cut.

Minor, not patch: config schema moves **v17 → v18** (`[knowledge] enabled`),
the CLI gains a new surface (`kb status|enable|disable`, TUI `/kb`), and two
KB API responses gain fields. **The schema migration is one-way** — after any
command of this release touches `config.toml`, older binaries refuse it.

Console pin moves to claw-ui **v0.3.17** (activation screen, honest empty
states, accurate upload accept list).

### Operators: read before upgrading

- **Every existing knowledge base must be re-embedded once.** Chunks are now
  embedded with their metadata prefix and tagged `<model>+meta1`; `kb drift`
  reports your whole pre-upgrade corpus as stale **by design**. Run
  `rantaiclaw kb re-embed --include-current` (calls your embedding provider
  once per chunk — costs money on paid providers). Rolling back the binary
  after a re-embed needs the matching `kb.db` backup restored with it.
- **The KB now has an explicit on/off switch.** Fresh installs start OFF; an
  install that already carries an embedding key (config or
  `KB_EMBEDDING_API_KEY` env) migrates **ON** automatically. While off, KB
  routes answer `503 kb_disabled` and the agent is not told the KB exists.
  Deactivating keeps your credentials; removing a key is a separate action.
- **A `kb.db` whose recorded embedding dimension disagrees with
  `KB_EMBEDDING_DIM` now refuses to open**, naming both values. That is a
  pre-existing corruption surfacing, not a regression: either restore the
  original dimension or start a new database.
- `KB_INTELLIGENCE_RESOLUTION=fuzzy` (documented but never implemented) now
  fails config load instead of silently behaving as `exact`.

### Added

- `[knowledge] enabled` (schema v18) with an intent-preserving migration;
  gateway `GET/PUT /config/knowledge` carries it, and activation live-probes
  the embedding key so a rejected credential is never persisted (mirrors the
  Telegram `getMe` pattern).
- CLI `kb status` (read-only — never rewrites `kb_meta`), `kb enable`
  (refuses without a resolvable key), `kb disable` (credentials kept); data
  subcommands answer a parseable `kb_disabled` TOON error with exit 1 while
  off. TUI `/kb` shows the same status and toggles.
- `KB_VISION_MODEL` — image ingestion's model is a config knob instead of a
  hardcoded constant (default unchanged: `openai/gpt-5-mini`).
- Model registry documentation: known-good embedding model/dimension pairs,
  the safe model-change procedure, and a per-provider verified compatibility
  matrix (`docs/reference/kb.md`, `docs/reference/kb-providers.md`).
- Graph API capability block now answers *why* a graph is empty:
  `credential_configured`, `graphrag_enabled`, `resolution` (additive).
- Re-extract reports `failed_chunks` + `error`, and answers `502
  extraction_failed` when every chunk failed — a total failure is no longer
  indistinguishable from a document with no entities.

### Fixed

- **Agent search output** carries the full retrieval context and the RAG
  citation-discipline block instead of 120-character previews; a zero-hit
  search states whether the scope is empty or nothing crossed the threshold.
- **Chunks are embedded with their Category/Topic/Section prefix** on all
  three paths (HTTP ingest, CLI ingest, bulk re-embed) — the pipeline's
  design intent, previously reaching zero of them.
- **GraphRAG can see LLM-extracted entities** (mentions were stored with a
  NULL chunk index the SQL join can never match) and **relations survive
  casing/punctuation mismatches** between the model's entity and relation
  arrays; unmatched relations are counted, not silently dropped.
- **Group-scoped vector search finds chunks that rank outside the global
  top-K** — the KNN is now constrained to the group's rowids (sqlite-vec
  0.1.9 `rowid IN`), so a small knowledge base in a large corpus no longer
  returns zero results.
- Graph node selection orders by the same deduplicated degree it displays;
  entity counts report the stored (deduplicated) set; group document counts
  and drift totals exclude soft-deleted documents, so `kb drift` can reach
  `in_sync` again after a console delete.
- Membership rows validate both the group and the document inside the insert
  transaction (foreign keys are off in SQLite here).
- A key entered in the console reaches **every** KB consumer — query
  expansion, contextual retrieval, and the LLM reranker read the unified
  `chat_api_key` instead of raw env; a credential-less reranker is skipped
  with one warning instead of erroring per query.
- The ambient KB hint follows the operator's intent (`enabled`), not the
  existence of `kb.db`; the KB context cache is keyed on the credentials, so
  a key changed from any surface takes effect without a restart.
- The KB 32 MiB upload body limit applies to the upload route alone — other
  KB routes return to the standard 64 KiB cap (unauthenticated callers could
  previously make the gateway buffer 32 MiB per request on any KB route).
- Saving a KB key no longer restarts the gateway mid-request.
- Contextual retrieval and the RAG instruction block, both shipped dead, are
  wired; the standalone query rewriter, dead with a live env knob, is
  removed (`KB_STANDALONE_QUERY_ENABLED` no longer exists).
- Hygiene: character (not byte) thresholds in OCR-fallback detection for
  non-Latin scripts; heading blocks no longer leak body text into section
  paths; hybrid-merge substitutes repeated phrases in document order; graph
  edge filtering pushed into SQL; LLM extraction runs concurrently
  (bounded by `KB_EMBED_CONCURRENCY`); honest error messages for the OCR
  feature gap; bare-word extractor sentinels fail at config load.

### Changed

- Console pin moves to claw-ui **v0.3.17**: the KB panel is an activation
  screen while the KB is off (no more stacked error panels), a rejected key
  shows inline on the form, Deactivate keeps the key, the graph/drawer empty
  states name the actual cause, and the upload picker only offers formats
  the backend accepts.

## [0.18.4-alpha] — 2026-08-10

One security fix (#444) and the console pin that completes it: an API key
stored in `api_url` was held on disk in plaintext and rendered in the web
console's base-URL field.

Patch rather than minor: no new surface and no config schema change, so this
rolls back freely. The one behaviour change worth knowing is that a
credential-shaped `api_url` is removed from `config.toml` on the next load —
see the note below, and rotate the key if this applies to you.

Console pin moves to claw-ui **v0.3.16**, which stops the base-URL field
holding a value the gateway has stopped returning. Without it the key stays
on screen until a page reload even after this release withholds it.

### Security

- **An API key stored in `api_url` is no longer kept on disk or shown in the web
  console.** `api_url` is written to `config.toml` in plaintext, unlike `api_key`,
  which is encrypted at rest. v0.18.0-alpha started rejecting credential-shaped
  writes at the config API, but that guard is write-only: every config that
  already held such a value kept it, and `GET /api/v1/secrets` returned it
  verbatim into the console's **API base URL** field — a plain-text input, not a
  password one — so the key was displayed in the browser.

  Observed on a real profile: `api_key = "enc2:…"` (encrypted) next to
  `api_url = "sk-or-…"` (an OpenRouter key, plaintext).

  Three layers now apply one rule — credential-shaped values are never stored,
  never echoed, always warned about:

  | layer | behaviour |
  | --- | --- |
  | `Config::load_or_init` | drops the value between read and parse, writes the cleaned config back, and warns naming the file and telling the operator to rotate the key |
  | `GET /api/v1/secrets` | withholds a credential-shaped `api_url` so the console cannot render it |
  | `rantaiclaw doctor` | new `config.api_url` check reports a value that is not a usable URL |

  A malformed-but-harmless value (a typo, a bare hostname) is deliberately
  *kept*: it is not a secret, and dropping it silently would hide the operator's
  mistake. That is the case the new `doctor` check reports.

  **Operators: if a key was ever stored in `api_url`, rotate it.** It was held
  unencrypted and rendered in the browser; removing it does not undo that.
  Restart the daemon after upgrading so the running process stops holding the
  old value in memory.

### Changed

- **Console pin moves to claw-ui v0.3.16.** The Providers form mirrored
  `GET /secrets` into the base-URL field behind a truthy guard, so it only ever
  filled the field and never emptied it. A value the gateway stopped returning
  stayed on screen until a page reload — which is exactly what the withholding
  above causes. Both fields also gained real labels; they sit next to each
  other, one takes a URL and one takes a credential, and pasting a key into the
  wrong one is what created the finding above.

## [0.18.3-alpha] — 2026-08-09

One fix: `doctor models` reported a passed check for a provider it could not
reach. This is the other half of the Ollama finding from v0.18.0-alpha — that
release made the probe able to fail, and left the verdict still saying it
passed.

### Fixed

- **A failed model probe no longer reports as a passed check.** `doctor models`
  printed `✅ model catalog check passed` for a provider it never reached.
  Observed on a machine with no Ollama running: the fetch failed, the command
  fell back to a previously-cached list, and the run summary counted it among
  the "ok". An operator had no way to learn their provider was down.

  `run_models_refresh` returned `Result<()>`, which collapsed four outcomes into
  one `Ok` — three of which involve no successful fetch. It now reports which
  one it hit, so `doctor models` can grade it:

  | what happened | before | now |
  | --- | --- | --- |
  | fetched live from the provider | `✅ passed` | `✅ passed (N models)` |
  | cache inside its TTL, no request made | `✅ passed` | `✅ cached catalog is fresh (provider not contacted)` |
  | provider returned an empty catalog | `✅ passed` | `⚠️ stale — nothing verified` |
  | the fetch failed outright | `✅ passed` | `⚠️ stale — provider unreachable, nothing verified` |

  The run summary gains a `stale` count, kept separate from `ok`.

  `models refresh` is unchanged: its job is to hand back a usable catalog, and
  falling back to a cached list still does that. Only the *diagnostic* command
  changes, because only it claims to have verified anything.

### Changed

- **`doctor models --provider <name>` now exits non-zero when that provider
  served only a stale cache.** It asks whether the provider is reachable, and a
  previously-cached list does not answer that. A reachable provider still exits
  zero. If you script this command, a provider that is intentionally offline
  some of the time will now be reported as a failure.

### Notes

- No config schema change, and the console pin stays at claw-ui `v0.3.15`. This
  release rolls back freely.
- `ModelRefreshOutcome` and `StaleCacheReason` are new public types, and
  `run_models_refresh`'s return type changed. All in-tree callers are updated;
  external callers of the library API would need to adjust.

## [0.18.2-alpha] — 2026-08-08

Four memory fixes. The first one changes what reaches the model, so read it even
if you skip the rest.

Every fix here was reproduced by a failing test before it was written, and each
regression test was checked by restoring the old behaviour and confirming the
test goes red — the counts below say which tests actually discriminate and which
only guard against the fix over-reaching.

### Fixed

- **Forgetting a core memory now removes it from the prompt too.** `MEMORY.md` is
  injected into every system prompt, and on the `sqlite` and `lucid` backends it
  is a *projection* of the `core` rows rather than the store itself. Only the CLI
  re-projected after a write. A memory deleted through the agent's own
  `memory_forget`, the TUI's `/memory remove`, or `DELETE /api/v1/memory/{key}`
  left the authoritative store and stayed in the file the model reads:

  ```
  <!-- rantaiclaw:memory:begin -->
  - rotation_note: staging credentials rotate weekly
  <!-- rantaiclaw:memory:end -->
  ```

  The projection is otherwise rebuilt only when a backend is constructed. For
  `rantaiclaw run` that is the next process; for the gateway and the TUI, both
  long-lived, it was the rest of the process lifetime — and a session started
  inside that process read the stale file. The store side was the mirror image: a
  `core` memory written mid-session was not in the injected file at all.

  All seven write paths now re-project. Eight of the ten new tests fail against
  the previous behaviour, one per call site. The `markdown` backend is
  deliberately excluded — it owns `MEMORY.md` directly.

- **The `markdown` backend replaces instead of appending, and forgets every
  copy.** Storing a key twice wrote two lines, which inflated `count()`, showed
  the key twice in `list()`, and left `get()` returning whichever copy sorted
  first. `forget` then stopped at the first file that matched, so a key present
  in both `MEMORY.md` and a daily log lost one copy, kept the rest, and still
  reported success — `memory_forget` answered "Forgot memory: k" about an entry
  `get()` would still return. Since that tool exists partly to delete sensitive
  data, a false success there is worse than a missing feature.

  `SqliteMemory` and `PostgresMemory` both upsert on `key`; `Memory` is one
  trait, so this was a contract divergence rather than a backend flavour. A
  re-store under a different category now moves the entry, matching what sqlite's
  upsert does to `category`.

  Only reached when `[memory] backend = "markdown"`; the default is `sqlite`.
  Existing duplicate lines collapse on the next write of that key — no migration.

- **`memory stats` no longer prints a total its own breakdown contradicts.**
  `Total:` came from `count()` while the `By category:` block was built from
  `list()`, which is capped at 1000 rows. Past the cap the two disagreed with
  nothing saying the breakdown was partial — 1100 stored entries reported
  `Total: 1100` above categories summing to 1000. The block is now labelled
  `(most recent N of M)` when the page is short, matching the wording
  `memory list` already used.

  The same command also swallowed a `count()` failure as `Total: 0`, which is
  indistinguishable from an empty store. `Health:` does not cover that gap:
  sqlite's health check is `SELECT 1` and survives a damaged `memories` table.
  It now reads `unavailable (<cause>)`.

  **If you parse this command:** `Total:` is no longer always an integer, and the
  category header can carry a suffix. It is a human-facing diagnostic, not a
  documented machine interface.

- **`memory_forget` consults the autonomy gate before it reads the store.** The
  `contains` selector resolves by scanning every entry, and that ran before the
  policy check. A read-only or rate-limited caller was answered out of memory
  contents — `'deploy' matches 2 memories (b, a); be more specific` — instead of
  being told it was refused, which is an instruction to retry a call that can
  never complete. Nothing was ever deleted; the gate did stop the mutation, just
  late. The scan also happened outside anything the rate limiter accounts for,
  since `enforce_tool_operation` is what records the action.

### Notes

No config schema change: `schema_drift` passes without a snapshot update, so this
release carries no migration and rolls back cleanly.

## [0.18.1-alpha] — 2026-08-08

**Nothing in this release changes how the binary behaves.** Both commits are
test-only; the shipped artifact is byte-for-byte equivalent in behaviour to
0.18.0-alpha. It exists so the fixes below are carried on a tag rather than only
on `main`. If you install RantaiClaw, you can skip it.

It matters if you run the test suite from source.

### Fixed

- **Unit tests no longer write into the operator's own `~/.rantaiclaw/`.** Five
  tests treated a `TempDir` as isolation while the code under them resolved its
  destination from the process `HOME`, so every `cargo test --lib` appended to
  real state:

  - Two wrote to `sessions.db`. `open_session_store` and
    `open_cli_session_store` resolve the session database from
    `ProfileManager::active()` — from `HOME` — not from the `Config` handed to
    them, so a synthetic `Config` isolated nothing. Each run left a
    `[cron:test-job cron-job] Say hello` session (dangling at
    `message_count = 1`, the provider having failed as the test asserts) and a
    `hello` / `test-model` session. On one machine these had reached 267
    sessions — 88% of that profile's history — and read as a runaway cron job
    rather than test residue.

  - Three wrote `active_workspace.toml`. `run_quick_setup_with_home` takes
    `home` as a parameter and every write honours it except the last:
    `persist_workspace_selection` resolves its destination from `HOME` instead.
    The marker landed in the real `~/.rantaiclaw/` naming a tempdir deleted
    moments later. That file precedes `~/.rantaiclaw/config.toml` in the
    resolution order, so a stale one shadows the real config — the split-brain
    `active_workspace_marker_is_temp_leak` already existed to catch, which is
    why the symptom was a warning on every CLI invocation rather than a broken
    install.

  Each fix pins `HOME` under the crate-shared `test_env::ENV_LOCK` **and asserts
  the pin took**, so it cannot rot into a no-op — which is exactly how a sibling
  test pinned in 0.12 kept looking fixed while the one beside it leaked. Removing
  either pin now fails its test with the real path in the message.

  Verified by row count against a real profile: an unpatched 0.18.0-alpha binary
  takes the session table 39 → 41 on those two tests alone; patched, 41 → 41, and
  a full `cargo test --lib` (4097 tests) leaves both the session database and the
  workspace marker byte-identical.

## [0.18.0-alpha] — 2026-08-07

The provider/model catalog, audited by diffing it against what the providers
actually serve rather than by reading it. Six changes; the security one first.

### Security

- **The model probe no longer prints `api_url` into its error output.**
  `rantaiclaw doctor models` printed a live provider API key in plaintext on its
  `[llamacpp]` line. Four independent links produced it and each is closed:
  `run_models_refresh` handed the *global* `config.api_url` to every provider
  probe, and `resolve_live_models_endpoint` trusts that argument unconditionally
  for llama.cpp — so a sweep built llama.cpp's endpoint out of whatever the
  active provider had stored there. The fetch error then echoed the endpoint
  verbatim. The value was **not transmitted** (an unparseable URL fails inside
  the HTTP client before a socket opens), but it reached stdout, and therefore
  terminal scrollback and CI logs.

  Endpoints are now parsed before the request is built and reduced to
  scheme + host + path in any error — `userinfo` and query strings are dropped,
  and a value that does not parse as a URL is withheld entirely.

  **If you ran `doctor models` on an affected build, rotate the key.** An
  operator whose `api_url` already holds a credential keeps that stored value
  until they next set it; nothing rewrites config on upgrade.

### Changed

- **`PUT /api/v1/secrets` now rejects an `api_url` that is not an http(s) URL,
  and one that looks like an API key.** Any non-empty string was previously
  accepted and written to `config.toml` **in plaintext**, while the `api_key`
  in the same request body is encrypted at rest. Validation runs before any
  mutation, so a rejected body leaves the whole config untouched. This is a
  contract change: a request that used to return 200 can now return 400.
- **A live model list is authoritative; the curated list is a seed.** The
  catalog previously appended every curated id the live list lacked. Five of the
  twelve curated OpenRouter entries were absent from that provider's live
  catalog — two of them (`meta-llama/llama-spark`, `openai/gpt-5.5-codex`) exist
  nowhere at all — so the union manufactured options that fail at call time.
  Once you have run `models refresh`, the list you see is the provider's own.
- **Providers with no curated catalog report an empty list** instead of a single
  entry literally named `default`, which was not a model id on any of them
  (`synthetic`, `opencode`, `doubao`, `copilot`, `lmstudio`, `ovhcloud`).
- **Default models changed for five providers.** Two pointed at models that do
  not exist: `venice` (`zai-org-glm-5.1` → `zai-org-glm-5-2`; Venice separates
  version parts with dashes) and `nvidia` (`meta/llama-spark` →
  `meta/llama-3.3-70b-instruct`; NVIDIA NIM serves no `llama-spark`). Three more
  were absent from their own provider's picker, so setup wrote a `default_model`
  the operator could not re-select: `ollama`, `llamacpp` and `bedrock`.
  Existing configs are not rewritten — a profile already holding one of these
  keeps it until you change the model.
- **Bedrock model ids use one format.** Its four-entry list carried three, two
  of which omit the `-v1:0` suffix Bedrock requires. Bedrock has no live model
  discovery, so curated is its entire catalog and a malformed id there can never
  be corrected by a refresh.
- Refreshed the OpenRouter, Venice, NVIDIA, astrai and Ollama curated lists
  against live catalogs from those providers. The setup wizard's provider-tier
  labels no longer name specific model versions (they advertised "GPT-4o, o1,
  o3", "Grok 3" and "Gemini 2.0 Flash & Pro" while the lists one file over said
  otherwise). Knowledge-base image ingestion moves off `gpt-4o-mini`.

### Added

- **`rantaiclaw models refresh --all`** refreshes every registered provider in
  one sweep, continuing past providers that lack a key or live-discovery
  support. This existed only as `doctor models` — the command nobody reaches for
  when they want to update a model list. `--all` conflicts with `--provider`,
  always forces a live refresh, and `--help` states that the sweep sends each
  provider's configured (or env-supplied) key to that provider's own endpoint.

### Fixed

- **The TUI `/model` picker reads the model cache.** It was built from the
  curated list alone, so `models refresh` and `doctor models` — both of which
  write `models_cache.json` — had no effect on the TUI at all. On a profile
  whose cache held 400 live OpenRouter models the picker still offered 12.
- **Anthropic completions no longer down-tier models the match arm never
  named.** `with_anthropic_max_tokens` enumerated the *high* tier and let
  everything else fall lower, so `claude-opus-4-8` received 64k instead of 128k
  and the Claude 5 family received 4096 — a 16x undershoot. Both truncate a long
  generation silently: the operator sees a short answer, not an error. Known
  lower tiers are now enumerated instead, and an unrecognized `claude-*` logs a
  warning rather than quietly taking the floor.
- **A remote-Ollama refresh performs a real probe.** That branch returned a
  hardcoded ten-entry list which was then cached and reported as
  `✅ model catalog check passed` — observed passing on a machine with no Ollama
  running. A probe that cannot fail is not a check.
- The channel runtime's in-chat `/model` reply no longer carries its own copy of
  the cache path, deserialization structs and lookup. All four surfaces — TUI,
  CLI, channels, gateway — now resolve one catalog.

### Notes

- **No config schema change**, and the bundled console stays at claw-ui
  `v0.3.15`. Like v0.17.1-alpha and unlike v0.17.0-alpha, this release rolls
  back freely.
- Two invariants were added to keep this class of drift from returning: every
  provider's default must appear in that provider's own curated list, and every
  Bedrock id must carry its version suffix. The only prior coverage exercised a
  single provider, which is why the defects went unnoticed. Correcting the data
  also required correcting three existing tests that had pinned the broken
  values in place.
- Not addressed: the cost pricing table (`get_default_pricing`) has zero overlap
  with any current default model, so enabling cost tracking leaves every model
  you actually use unpriced. `[cost] enabled` is `false` by default, so nothing
  is broken today. Tracked for a follow-up that can verify real prices.

## [0.17.1-alpha] — 2026-08-06

Two defects on the update path itself, found by taking v0.17.0-alpha on a real
install rather than by reading the code.

### Fixed

- **A successful update no longer ends in a fatal-looking error.** After
  printing `✓ updated` and `✓ daemon service restarted`, the run finished with
  `Error: Failed to migrate config schema`. Everything had in fact worked; the
  error came from the post-update console notice, which spawned a child with
  inherited stderr. That child loads config on its way to the check, moments
  after the binary was replaced, so it can fail for reasons unrelated to the
  update that just succeeded. The last thing on screen was `Error:` directly
  under a line explaining how to roll back — inviting an operator to undo a
  working update. The notice is advisory and now stays silent when it cannot
  run.
- **`--backup` produced no tarball.** It wrote the archive inside the directory
  tree it was archiving, so `tar` read the file it was still writing and failed
  the whole run with "file changed as we read it" — reported only as a `⚠`,
  while what was lost is the artifact you would restore from. The snapshot
  directory is now excluded from the archive.

### Notes

- No config schema change, and the bundled console stays at claw-ui `v0.3.15`.
  Unlike v0.17.0-alpha — which migrated the config to schema v17 and is
  therefore one-way — this patch can be rolled back freely.

## [0.17.0-alpha] — 2026-08-06

The memory subsystem, audited by running it rather than reading it.

Most of what follows was found by driving the binary against a real store —
not by reading code, and not by a failing test. Several defects returned
confident wrong answers, which is why a green suite had never caught them.

### Added

- `memory reindex` re-embeds entries the current embedding model cannot use,
  so changing embedding provider or dimensions no longer silently disables
  vector search for everything stored before the change.
- `GET /api/v1/memory` accepts `category` and `q`. `q` routes through the
  backend's ranked recall and composes with `category`; entries carry `score`
  when a search ranked them. Both are additive — absent behaves as before.
- `POST`, `GET /{key}` and `DELETE /{key}` on `/api/v1/memory`: the console
  had been calling write endpoints that did not exist.
- The agent reports which stored memories shaped a turn. The TUI prints
  `↺ recalled N memories: …`; the console shows them as chips beside Sources.
  Emitted only when something was actually injected.
- `/memory stats` in the TUI, and `--category` on `/memory add` — the CLI, the
  API and the console could all pick a category; the terminal could not.

### Fixed

- **A turn no longer recalls the question it is answering.** Auto-save writes
  each user message to memory before recall runs, so the store held a verbatim
  copy of the question. Being a perfect lexical match it took the top rank, and
  scores normalise *relative to the best hit* — so one self-echo pushed every
  curated fact under `min_relevance_score`. Measured on a live store, the same
  query returned one entry (its own question) with auto-save on, and five
  curated facts with it off. Now the echo is dropped **before** the threshold
  and the survivors are re-ranked.
- `/memory recall project 2024` searched for `project`. A trailing token was
  parsed as a positional limit, so any query ending in a number silently lost
  it. Limit is now `--limit N`.
- `/memory list <category>` reported the whole store's count beside a filtered
  body — `(121, listing the most recent 1)`. A filtered list now reports the
  filtered count.
- One conversation's memory no longer reaches another's prompt on a shared
  channel.
- Memory backends now honour the contract they advertise: `forget` really
  forgets on markdown, and an unrecognised `backend` is a startup error rather
  than a silent fallback to a different store with different semantics.
- The memory schema was declared twice and had drifted; the sqlite backend
  could fail to open. There is now one definition.
- Narrow terminals no longer clip the fields that matter most — the autonomy
  indicator used to become an ambiguous single letter at 60 columns.

### Changed

- Relevance scores are normalised per result set on every backend, so
  `min_relevance_score` means the same thing everywhere: "at least this
  fraction as good as the best hit".
- Timestamps are stored in UTC and rendered to second precision.
- `MEMORY.md` is a one-way projection of the store's core memories, written
  between markers; anything outside them is preserved byte-for-byte.
- The response cache was removed. It was never wired up, and its key had no
  scope dimension.
- Config schema is now v17 (the response-cache keys are migrated away).
- Bundled console moves to claw-ui `v0.3.15`: the memory panel gains search,
  category filtering and paging — every entry is reachable, where it
  previously stopped at 100 with no way forward.

### Notes

- Channels inject memory context only on a conversation's **first** turn. This
  is pre-existing behaviour and is unchanged here; it is documented so it is
  not mistaken for a regression.
- The self-echo fix changes what every turn sends the model. That is the point
  of it, and it is the blast radius.

## [0.16.4-alpha] — 2026-08-03

The terminal's skill screens, audited by driving them rather than reading them.

### Fixed

- **A failed ClawHub search was silent.** The error was logged and the previous
  results were left on screen — indistinguishable from a search that succeeded
  and returned the same thing. A failure you cannot see is one you act on. It
  now says so beside the title, and names the root cause (`Connection refused`)
  rather than our own "GET clawhub search" context.

- **A search gave no sign it was running.** Six to sixteen seconds against the
  live registry with the stale list still showing and nothing moving; you could
  not tell whether Enter had registered. Installing already animated a spinner,
  so searching — the slower of the two — was the one without feedback.

- **`↓` bounced between the last two entries of a long list.** The end-of-list
  wrap returned to the top of the last *page* instead of the top of the list.
  Latent for as long as a page was the whole screen; reachable once a two-row
  entry halved the stride.

- **Rows were clipped mid-word at the border.** They are cut with an ellipsis
  now, so a cut reads as a cut. The fit is measured in display cells rather
  than characters — a Chinese summary occupies twice the width a character
  count budgets for it, and ClawHub returns those.

- **Literal `\n` escapes appeared verbatim** inside ClawHub summaries, where a
  publisher had written them into their own frontmatter.

- **The skill detail panel explained every skill with a summarizer's example.**
  `weather` suggested `e.g. summarize today's standup notes`. It names the
  skill in front of you now, and gained `Source`, `Folder` and `Tools`.

- **`/skills new` and `/skills edit` did nothing.** Only the singular `/skill`
  accepted them, so the plural fell through to the picker, which preselected
  nothing and said nothing. Both spellings route to the same handlers now.

### Changed

- **`/help` shows each command's invocation form.** Two dozen commands
  implement `usage()` and nothing rendered it — its only readers were two error
  paths — so `/skill new`, `/skill edit` and `/skill install` were documented
  in a string no user could reach. Commands that take no arguments still show
  their bare name.

- **Pickers and info panels size to their contents** instead of taking the
  whole screen. Five skills used to sit at the top of a forty-six-row box.

- **Installed skills show where they came from** — `yours`, `@publisher`,
  `bundled`, `git`, `local`. After installing `@steipete/weather` the row read
  simply `weather`, so the publisher you chose on the install screen was gone
  by the time you looked at what you had. ClawHub rows are marked `installed`
  or `@other installed`, since the gateway refuses to overwrite another
  publisher's directory and an install that cannot happen should not be
  offered.

### Notes

Terminal-side only. No gateway, API, or configuration change, and the console
pin stays at claw-ui v0.3.14.

Two of the tests written for this shipped green against the bug they were
meant to catch: ratatui clips at the border whatever it is handed, so
asserting that a row stops there passes for a hard mid-word cut too. They were
found by mutating the fix away and watching them stay green, and every test
here was re-checked the same way.

## [0.16.3-alpha] — 2026-08-03

You could not type a space into the skill authoring form. This fixes that, and
rebuilds the Skills panel around it.

### Fixed

- **A space could not be typed into the skill form.** `Kopi Pagi` came out as
  `KopiPagi` — in Name, in Description, and in every Instructions step. Any
  multi-word value was unreachable, which is most of them.

  The form deliberately holds no state of its own: each keystroke writes the
  whole field into the `SKILL.md` document and reads it straight back, which is
  what lets the Form and Markdown views share one source and keeps
  hand-written sections intact. But both halves of that round trip normalized
  — the writer trimmed, and so did the reader — so a trailing space was
  deleted before the next character could arrive after it. Normalization
  belongs at save, not at every keypress; only line breaks are stripped now,
  since those are the one thing that genuinely breaks a line-oriented
  frontmatter parser. Whitespace at the ends never mattered: the loader trims
  it either way.

- **`Add step` did nothing once a step existed.** Same defect from the other
  direction — the writer filtered empty items out, so a newly added blank row
  disappeared before it could be typed into, and clearing a step's text
  deleted the row out from under the cursor.

### Changed

Ships as claw-ui v0.3.14, which this release pins.

- The left rail reads **Skills**, not *ClawHub Skills*. Installed skills also
  come from bundled packs, git, local paths, and the user's own hand; naming
  the marketplace made the section look like it was only for browsing someone
  else's work. Route ids are unchanged, so `#skills` still resolves.

- The Skills panel is rebuilt around the authoring form. `Write` is reachable
  from both views instead of only Installed — it used to disappear exactly
  when a user with nothing installed most needed it. The installed list gained
  search and origin badges (`yours`, `@publisher`, `bundled`, `git`, `local`),
  the ClawHub error state gained the retry it never had, and the editor moved
  from a modal to a drawer carrying a running size against the gateway's 64 KB
  body cap — a limit that previously surfaced only as a `413` after a failed
  save.

- Smaller corrections in the same pass: `Needs a name.` no longer greets an
  untouched field, labels are bound to their inputs, `Enter` continues the
  step list, the uninstall prompt names the skill rather than its directory,
  and the nav badge follows your own writes instead of reporting the count
  from page load.

### Notes

The gateway is untouched — `/api/v1/skills*` is unchanged, and this is a
console pin bump plus its release.

The regression test types character by character through the same write/read
cycle the component performs. That is the only shape in which this reproduces:
asserting on a single write passes either way, which is why the existing suite
was green throughout.

## [0.16.2-alpha] — 2026-07-31

The skill authoring form shipped unreachable. This makes it work.

### Fixed

Three defects in the web console's skill editor, all found by driving it with
a real browser rather than reading the code. Ships as claw-ui v0.3.13, which
this release pins.

- **The form never opened when creating a skill.** Clicking `Write` went
  straight to the markdown view, under a message saying the file's structure
  had been changed by hand — about a template the editor had generated itself
  a moment earlier. The check deciding whether the form can be used rejected a
  blank `name:`, which is exactly what a new skill starts with. The form,
  which is the feature, was unreachable.

- **The title heading was written blank.** Typing a name updated the
  frontmatter but not the `#` heading, so skills created through the console
  landed on disk with a bare `#` as their visible title. That heading is part
  of the body the model reads, so it was not only untidy.

- **The pencil always opened the markdown view.** The editor's view was stored
  state pushed by an effect: while a skill's content was still loading the
  document was empty, empty does not parse, so the effect switched to markdown
  once and nothing ever switched back. A momentary condition latched
  permanently.

### Notes

None of the three was reachable from the unit suite as it stood. The first had
a test that passed a name the editor never supplies, so the failing path was
never exercised; a second test asserted the buggy behaviour outright. The
third lived in the ordering of an async fetch against a React effect, which no
function-level test can observe.

A skill created through v0.3.12 has a blank `#` title on disk. Harmless, and
editing and re-saving it now fixes it.

## [0.16.1-alpha] — 2026-07-31

Four providers could be configured but never answered, `doctor` blamed your
network for it, and setup was sending API keys to two domains that do not
exist.

### Security

- **Onboarding sent freshly-entered API keys to unregistered hostnames.** The
  setup flow validates a key by sending it as a bearer token to a
  `/v1/models` URL. The host came from a table in
  `onboard::provision::provider` that had drifted from the one the client is
  actually built with — and two of its entries named domains that do not
  resolve: `api.zPUmlw.com` for Z.AI and `api.moonshot.io` for Moonshot
  International.

  Nothing leaked: DNS failed before any connection, so those keys were never
  transmitted and **no rotation is warranted on account of this**. The
  exposure was latent — either name could be registered by anyone, and from
  that moment every setup run selecting those providers would have handed over
  a working key.

  The table is gone. Setup now resolves the endpoint from the same source the
  provider client uses, so the set of hosts that can receive a key is exactly
  the set the agent already talks to. Where no endpoint is known the key is
  saved unvalidated with a message saying so, rather than sent somewhere
  guessed.

  Three further entries pointed at the wrong host without being fictional:
  `vercel`, `glm`, and `cohere`.

### Fixed

- **groq, xAI, Venice, and Vercel AI Gateway sent every message to a 404.**
  `OpenAiCompatibleProvider` appends `/chat/completions` to the configured base
  URL verbatim, and these four shipped with a base missing the path their
  vendor serves on. They could be selected, shown as ready, and then failed on
  every single request with no indication why.

  | provider | was | now |
  |---|---|---|
  | groq | `api.groq.com/openai` | `api.groq.com/openai/v1` |
  | xai | `api.x.ai` | `api.x.ai/v1` |
  | venice | `api.venice.ai` | `api.venice.ai/api/v1` |
  | vercel | `api.vercel.ai` | `ai-gateway.vercel.sh/v1` |

  Two were not simply missing `/v1`: Venice serves on `/api/v1` (the `/v1`
  path answers 402), and Vercel's gateway is on a different host entirely.

- **`doctor` turned a provider's name into a URL.** Its endpoint table held 8
  of the 33 providers in the catalog, and anything absent fell through to
  using the provider name itself as an address — `minimax` became
  `minimax/models`. reqwest cannot build a request from that, and its refusal
  was reported as `network error: builder error`, pointing you at your
  connection when nothing had been sent. Roughly twenty providers hard-failed
  this way on a healthy install.

  The check now says what is true: a provider it has no endpoint for is
  reported as not probed, rather than probed wrongly. Region-varying families
  (minimax, GLM, Moonshot, Qwen, Z.AI) resolve from the same constants the
  client is built with, so `minimax` and `minimax-cn` no longer collapse to
  one endpoint.

- **`doctor` probed GLM on the wrong host.** `glm` and `zhipu` are the same
  provider under two names and the client talks to `api.z.ai`, but the check
  probed `open.bigmodel.cn` — so a green tick there proved nothing about the
  configured client. Only the explicit `-cn` aliases belong on bigmodel.

### Notes

Recorded rather than guessed at: `qianfan`'s base (`aip.baidubce.com`)
redirects to a Baidu error page, so it is wrong too, but the replacement its
URL pattern suggests was not confirmed against documentation. Four further
providers — cloudflare, doubao, together, cohere — answer identically on every
path, so unauthenticated probing cannot establish whether their base is
correct; each needs a real key to confirm.

## [0.16.0-alpha] — 2026-07-31

Write and edit your own skills — from the web console, from the TUI, or by
hand.

Skills could be created from chat (`author_skill`) and nowhere else. Anyone
who wanted to sit down and write one, or fix a typo in one they wrote last
week, had to edit files by hand and hope. Both surfaces now offer it, and
both refuse to touch a skill someone else manages.

### Added

- **A `Write` button and a per-card pencil in the web console.** The editor
  has two views over one document: a form for name, description, tags, and
  instructions, and the raw markdown. Its entire state is the `SKILL.md`
  text — the form reads values out of it and patches them back — so moving
  between views cannot lose a section you wrote by hand. When a file's
  structure no longer matches what the form can locate, the form view is
  hidden rather than shown broken, and editing continues in markdown.
  Requires claw-ui v0.3.12, which this release pins.
- **`/skill new "<name>"` and `/skill edit <name>` in the TUI**, handing the
  file to your `$EDITOR`. No form: TUI users already have an editor, and a
  form there would be a worse one inside it. It also means pasting a prepared
  `SKILL.md` finally works — the TUI is suspended while the editor runs, so
  the composer's paste handling is not involved at all. `/skill edit` accepts
  either the display name or the directory name.
- **Three gateway routes carrying the console editor**:
  `GET`/`PUT /api/v1/skills/{slug}/content` and `POST /api/v1/skills`. All
  three are owner-scoped like their siblings *and* refuse any skill you did
  not author, with `403`. That gate is load-bearing: a skill's whole file
  becomes part of the agent's system prompt on the next load, so a route that
  rewrites one rewrites the agent's standing instructions.
- **`.origin.json`, recording who put each skill on disk** — `authored`,
  `clawhub`, `bundled`, `git`, or `local`. Written at every install path.
  Skills that predate it are classified from their directory's shape, and
  that inference is replaced by a real marker the first time the skill is
  saved. `GET /api/v1/skills` reports it as `origin`, alongside a new `slug`.

  This is what the edit affordances gate on. Before it, a skill you wrote, a
  bundled pack member, and a folder a third party copied in were
  **byte-identical on disk** — so there was no honest way to decide whether
  Edit should appear. Editing the wrong one loses the work silently: bundled
  skills are re-seeded by the next `setup` run, vendor-managed ones by their
  installer.

### Fixed

- **The API could create a skill it could not then disable or delete.** Skill
  routes were addressed by manifest `name`, which is free text, and
  `validate_slug` — which they run it through — rejects spaces. So a skill
  called `Kopi Pagi` answered `400` on `PUT .../enabled` and `DELETE`, while
  `GET`, which has no such guard, worked. The CLI never had the bug. Latent
  until now because ClawHub slugs are already slug-shaped; this release ends
  that, since every console-authored skill gets a human display name. All
  skill routes now address by directory slug, with a name fallback on the
  read routes so existing clients keep working. The `[skills.entries.<name>]`
  config key is unchanged — the routes resolve a slug and hand the name down.
- **A skill with a blank `description:` was described as `---`.** The
  frontmatter parser drops empty values, so the lookup missed and the
  fallback scanned from the top of the file and returned the frontmatter
  fence. Cosmetic in `skills list`, but not only cosmetic: the description is
  what the model reads to decide when a skill applies.
- Web console `Disable` and `Uninstall` now work on skills with a display
  name containing a space — the client half of the addressing fix above.

### Notes

Not included, and deliberately: skills still live in two directory roots
(`profiles/<p>/skills/` and `<workspace>/skills/`), and the write side now
targets only the first. Consolidating them, and the third-party installer
work that depends on it, is a separate effort.

## [0.15.2-alpha] — 2026-07-31

Make the always-on core skill actually always on, and stop a profile's paths
ignoring the profile.

### Fixed

- `owner-permissions` is installed on the setup paths users take. It is the
  sole always-on core skill — it teaches the agent the owner/guest model
  behind `manage_permissions` and `issue_pairing_code`, both of which are
  registered unconditionally, so without it the tools work but the agent has
  no manual for them. Only the headless section installed it, and `setup`
  reaches that section only when stdin is not a terminal; with one it launches
  the TUI overlay instead. The skill described as always-on was, in practice,
  almost never installed.
- Configuring a channel installs it too. `section/channels.rs` had always done
  this once a multi-user channel existed — "even if the skills section was
  skipped" — but none of the fifteen channel provisioners did, and those are
  what the TUI runs. A multi-user channel is the whole reason those tools
  exist, so this was the case that mattered most.
- A `Profile`'s paths resolve from its own `root` instead of re-deriving them
  from `name` through the global home on every call. Production layout is
  unchanged and a test pins that equivalence; what changes is that a profile
  pointed at a scratch directory now stays there. The provisioner smoke tests
  had been leaving a `test` profile in the developer's real `~/.rantaiclaw`,
  visible in `rantaiclaw profile list`, with contents that depended on what had
  run before.

No config keys, no API surface, and no CLI syntax changed.

## [0.15.1-alpha] — 2026-07-30

Remove a skills-onboarding branch that could never execute.

`SkillsSection::run` carried an interactive path calling into a dialoguer flow
that browsed ClawHub, multi-selected from the listing, and asked which
publisher a shared slug meant. Nothing could reach it: `setup <topic>` matches
a provisioner first and returns, so `setup skills` runs the provisioner and
never touches the section, while a bare `setup` reaches it only when stdin is
not a terminal — and with a terminal the TUI setup overlay runs instead. The
branch was therefore entered only where dialoguer has no terminal to prompt
on.

That left three implementations of skills onboarding for one job. This removes
the one that cannot run, along with `clawhub::install_many` and its report
types, whose only caller it was.

No behaviour changes. ClawHub browsing during setup was already directed to
the TUI's `/skills install`, by both the provisioner and the overlay, and
headless installs already work through `rantaiclaw skills install @owner/slug`.

The publisher prompt added to that branch in v0.15.0-alpha was real code on an
unreachable path; the parts of that release that are live — publisher-qualified
references, the `409` candidate list, per-skill provenance, and the agent
tools — are untouched.

## [0.15.0-alpha] — 2026-07-30

Make ClawHub skills installable again, and make it clear whose code you are
installing.

ClawHub now namespaces skills per publisher, so a bare slug no longer
identifies a skill: `GET /skills/<slug>` answers `409 AMBIGUOUS_SKILL_SLUG`
when several people publish it. That is 18 of the top 20 slugs, and the client
only ever sent bare ones — so installing `weather`, `github`, or
`obsidian` failed on every surface, with an error that read
`clawhub returned status 409 Conflict` and said nothing about why.

### Added

- `skills install` and `skills inspect` accept a publisher-qualified
  reference (`@steipete/weather`) alongside a bare slug. The owner is sent as
  `?owner=` on every endpoint an install walks.
- An ambiguous slug now reports the candidate publishers, annotated with
  install counts and ClawHub's official marker so the choice is an informed
  one. The TUI and the setup wizard turn that list into a prompt; the web
  console into a picker.
- `POST /api/v1/skills/install` answers `409 ambiguous_skill_slug` with a
  `matches` array instead of a generic `500`. Each entry carries a
  `reference` the client can send straight back.
- `GET /api/v1/skills` reports a `clawhub` object per installed skill (owner,
  slug, version, reference), read from a new `.clawhub.json` marker. Omitted
  when unknown — absence means unattributed, not "not from ClawHub".
- `skills_search` returns a ready-to-use `reference`; `skills_install`
  accepts it and, on an ambiguous slug, answers with the exact references to
  retry.

The publisher is never chosen automatically. Installing a skill stages remote
code the agent will later read and act on, and popular slugs attract
look-alike forks — one of the four `weather` publishers is a verbatim copy of
the top one, same display name, same summary.

### Fixed

- `skills update` re-fetches from the publisher a skill was installed from,
  rather than resolving the slug again and possibly swapping authors.
- Installing over a slug held by a different publisher is refused instead of
  reported as already installed.
- Batch installs report their failures. They went to `tracing::warn!`, which
  no console renders, so the onboarding wizard printed an empty
  "Installed from ClawHub:" line and no reason — the common outcome, since
  the listing it browses reports no publisher.
- The TUI install picker distinguishes same-slug results, installs the one
  shown, and no longer discards the publisher prompt on a keystroke.
- The picker's hint describes what its keys actually do, and PgUp/PgDn move a
  screenful instead of a fixed five rows.
- The web console marks installed per publisher, and its "official" badge no
  longer shares a colour with "installed".

### Changed

- Bundled console pinned to claw-ui `v0.3.11`.
- Removed the alt-screen render paths orphaned when chat moved to the
  alt-screen: they had no callers and carried a second, better-looking picker
  renderer that never drew.

## [0.14.0-alpha] — 2026-07-29

Make an autonomy change reach every surface without a restart. `[autonomy]`
had ten fields; only two could be refreshed on a running process, each through
its own bespoke override slot. The other eight were read straight off the
struct and stayed frozen at whatever was on disk when the process started —
so editing them saved to config and changed nothing.

Minor bump: cron tools no longer accept an `approved` argument, the Strict
preset now refuses `shell` at the gate rather than removing it from the
registry, and scheduled cron jobs are subject to the risk gate for the first
time. No config schema change (stays 16); the claw-ui pin moves to `v0.3.10`.

### Fixed

- **The whole `[autonomy]` section now refreshes on a running policy.** All ten
  fields move into one slot swapped as a unit, so a reload reaches
  `forbidden_paths`, `workspace_only`, `block_high_risk_commands`,
  `require_approval_for_medium_risk` and the budgets — not just `level` and
  `allowed_commands`. Process state (the rate-limit window, `/allow` grants,
  the approval registry) is deliberately excluded and survives a refresh.
- **The rate limit is enforceable on the gateway again.** The web console
  rebuilt its policy every turn, which reset the hourly window with it, so
  `max_actions_per_hour` never tripped there — a budget of 20 allowed 200
  actions. Regression shipped in 0.13.0-alpha.
- **The channel approval gate follows a live autonomy change.** It cached the
  boot level, so tightening from Full to Supervised left the gate open until a
  restart. Fail-open on the path where an arbitrary chat sender is the caller.
- **Switching between Manual and Smart takes effect on channels.** Both are
  `Supervised` and differ only by `always_ask`, which was cached at
  construction, so the switch was a no-op — and Smart to Manual is a
  tightening.
- **The channel system prompt no longer briefs the model on a stale preset.**
  The safety section is re-rendered per turn against the live policy, so the
  gate and the briefing move together.
- **The approval prompt names the command the gate actually rejected.** It read
  the boot allowlist while the gate read the live one, so after a reload that
  narrowed the list it named the wrong command — approving it would not have
  helped. `/allowlist` showed the same stale list, now relabelled "Config
  allowlist".

### Security

- **Cron tools can no longer approve themselves.** `cron_add`, `cron_run` and
  `cron_update` exposed `approved` as a tool parameter, so the model filled it
  in: a call refused for needing explicit approval could be re-sent with
  `approved: true` and pass. The parameter is gone from all three schemas.
- **Scheduled cron jobs now pass the risk gate.** The scheduled path checked
  autonomy, the allowlist and forbidden paths but never the risk
  classification, so a job the same policy would refuse from an interactive
  turn ran unchallenged at fire time.

### Changed

- **Strict refuses `shell` at the gate instead of unregistering it.** The tool
  stays listed and is denied on call with a policy reason, so the model reports
  the real cause rather than "I don't have that capability" — and leaving
  Strict mid-session restores it without a rebuild. Strict enforces the same
  set of refusals as before; the two characterization tests exist to prove it.

## [0.13.0-alpha] — 2026-07-27

Make the autonomy preset mean the same thing on every surface. A preset
switched in the TUI was invisible in the web console because the two wrote
the same four-rung ladder into `config.toml` with different encodings. Minor
bump: `strict` now enforces `readonly` rather than `supervised`, which is a
real change in what that preset blocks. No config schema change (stays 16);
the claw-ui pin moves to `v0.3.9`.

### Fixed

- **Autonomy preset encoding is now shared between the TUI/CLI and the web
  console.** `rantaiclaw autonomy <preset>`, the TUI's `/autonomy` and
  Shift+Tab now write `[autonomy].always_ask` alongside `level`, which is the
  pair the console reads. Previously `manual`, `smart`, and `strict` all
  collapsed onto `supervised` with `always_ask` untouched, so the console
  resolved every TUI-set preset to "Manual" and a switch made there produced
  no visible change at all.
- **`strict` enforces read-only.** It maps to `AutonomyLevel::ReadOnly`
  instead of `Supervised`, so every tool gated on `can_act()` is refused —
  file writes, `http_request`, browser, cron, tasks, `memory_store`, ssh,
  pty, skill installs — not just `shell` being unregistered. This matches
  what the preset already claimed, what the docs described, and what the web
  console has always sent for that rung.
- **The Strict system prompt describes the whole refusal.** It previously
  mentioned only `shell`, so the model kept offering writes and fetches the
  policy then refused. It also no longer asserts `shell` is absent from the
  tool list — a preset switched mid-session can leave it listed while every
  command is denied.
- **Setup now applies the approval preset it asked you to pick.** Both
  onboarding paths wrote `<profile>/policy/*.toml` and stopped; the runtime
  gate reads `config.autonomy`, so a fresh install that chose Strict, Manual,
  or Off ran under the default `supervised`. The marker is now mirrored into
  the config, read back from disk so an existing policy file wins over the
  offered preset.
- **The gateway's webhook path follows autonomy changes without a restart.**
  Its tool registry was built once at startup, pinning every tool's
  `SecurityPolicy` to the boot level. Because `ApprovalManager` treats
  `readonly` as blocked elsewhere, the strictest setting failed *open* — no
  approval prompt and a boot-pinned `can_act()` that still said yes. The
  registry is now built per turn, matching the console chat path, which
  already constructed a fresh agent per request and was never affected.

### Changed

- `[autonomy].always_ask` is now written by preset switches and by
  `rantaiclaw setup approvals`. `smart` clears it, which drops the default
  `ssh`/`pty` always-ask pair — the encoding the web console has always
  written for that rung.
- Documented `[autonomy]` defaults for `auto_approve` and `always_ask`
  corrected; both were listed as `[]`.

### Notes

- Existing installs keep their current `config.toml` until the next preset
  switch or `setup approvals` run; nothing is rewritten on upgrade.
- Operators parked on `strict` who relied on file writes or network tools
  should move to `smart`.
- The companion console fix ships in claw-ui `v0.3.9`; run
  `rantaiclaw ui update` to pick it up.

## [0.12.0-alpha] — 2026-07-25

Keep the optional web console (claw-ui) in sync with the binary — without
forcing it. Adds a proper `ui update` command and non-intrusive "a newer
console is available" notices. Minor bump: new `ui update` CLI subcommand
(runtime-contract addition). No config schema change (stays 16); the claw-ui
pin is unchanged (v0.3.8).

### Added

- **`rantaiclaw ui update`** — refresh the web console to this binary's pinned
  claw-ui release. Idempotent (a no-op "already up to date" when the installed
  version matches the pin) unless `--force`; `--check` reports availability
  without downloading. Clearer than re-running `ui install` to update.
- **Web-console version marker** — `ui install`/`ui update` record the installed
  claw-ui tag in a `.version` file, so drift against the binary's pin is
  detectable.
- **Freshness notices (non-forcing)** — `ui start` offers to update a lagging
  console (an interactive `[y/N]` prompt on a TTY, a one-line notice otherwise),
  and a successful `rantaiclaw update` reports when a newer console is available.
  Neither auto-installs — updating is always an explicit `ui update`.

## [0.11.0-alpha] — 2026-07-25

CLI usability and polish. Three new read commands (`integrations list`, `models
list`, `config show`), a batch of display and honesty fixes across the command
surface, and a consistent minimal-mono restyle of the main read commands. Minor
bump: new CLI subcommands (runtime-contract additions). No config schema change
(stays 16).

### Added

- **`integrations list`** — browse all integrations grouped by category; the bare
  `integrations` command now defaults to this instead of erroring with a usage
  message.
- **`models list [--provider <ID>]`** — view a provider's cached/curated model
  catalog (default model marked) with no network call.
- **`config show`** — print the active configuration as JSON with every secret
  redacted, reusing the gateway's two-layer secret scrubber.

### Fixed

- **Logs no longer pollute stdout** — on non-TTY/piped runs the tracing subscriber
  writes to stderr, so `rantaiclaw <cmd> | …` output stays clean for scripts.
- **`status` lists all 16 channels** — driven by a shared roster so it can no
  longer disagree with `channel list`; unconfigured channels show a neutral `○`
  instead of a red `❌`.
- **`cron list` renders schedules cleanly** — a `Display` impl for `Schedule`
  replaces the raw `Cron { … }` debug dump.
- **`autonomy` (no-arg) reports the enforced level** from `config.toml` (matching
  `status`/`doctor`) and warns when the preset marker has drifted — display only;
  enforcement was always driven by `config.autonomy.level`.
- **`insights` can't panic** on a short or non-ASCII latest-session id.
- **`channel add`/`remove` help is honest** — states that CLI add/remove redirect
  to `onboard`/config-edit and lists all 16 channel types.

### Changed

- **Minimal-mono CLI restyle** — a shared `cli_style` layer gives `status`,
  `autonomy`, `cron list`, `models list`, `channel list`, `insights`,
  `session list`, `skills list`, and `permissions show` consistent dim section
  headers, aligned fields, and `●`/`○` status dots. Color is applied only on a
  TTY; piped output stays ANSI-free.

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
  - PDF/markdown/image ingestion) moved into the default feature set, so a
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
  - tail the log file. (`src/channels/mod.rs` lines ~1324, ~1374,
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
  - ClawHub both write to the new path; the picker was reading from
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
