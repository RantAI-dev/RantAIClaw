# Config + Lifecycle CLI Deepscan — Full Findings Inventory

Audit date: 2026-08-27. Commit: `0e5fcc9`. Scope: Configuration panel + `config_api.rs`,
`src/config/`, CLI `onboard/setup/gateway/daemon/service/migrate/doctor/auth`, TUI
`/config` `/setup` + `first_run_wizard`, gateway lifecycle. Out of scope: MCP subsystem
(parked), `src/peripherals` internals, chat/persona gateway routes (audited v0.25.0).

**Status: findings collected + crown items vetted. NO plans written yet.** This file is the
completeness checklist before planning — every raw finding from all 8 audit agents is listed,
so nothing is lost when plans are drafted. Plans start at number **232** (231 is the highest existing).

Total raw findings: **125** (BUG-CFG 16 · SEC 12 · BUG-OB 20 · BUG-LC 17 · UI 17 · DEBT 8 · PERF 3 · TEST 17 · DOCS 8 · DX 3 · DIR 4).

Confidence/verified: ✓ = opened the cited code and confirmed directly this session. Others carry the agent's stated confidence (mostly HIGH).

---

## A. Security — credential leakage (HIGH)

- **A1 👑✓** [BUG-OB-01] `doctor models` / `models refresh --all` sends the ACTIVE provider's api_key to every provider in the sweep (incl. Google as URL `?key=`). `wizard.rs:2143→2146`; URL is gated (`active_provider_api_url` 1712), key is not. Fix: `resolve_key_for_provider` per target. Rotate any key swept.
- **A2** [SEC-01] `fetch_gemini_models` puts key in query string; on probe failure `reqwest` Display + `format_error_chain` prints the URL (key) to terminal/CI. `wizard.rs:1608`; `legacy.rs:872`. Also `providers/gemini.rs:500`.
- **A3** [SEC-03] Telegram doctor probe builds `.../bot{token}/getMe`; network/decode error carries the token in the URL into the rendered message. `checks/channels.rs:235`. Sibling Discord/Slack safe (header auth).
- **A4** [SEC-06 = UI-04] `GET /config` returns `api_url` and `mcp_servers.*.env` that `GET /secrets` deliberately withholds. `redact_config_secrets` (`config_api.rs:179`) never touches either; `is_secret_key` suffix list misses them. ✓ (confirmed redactor omits both).
- **A5** [UI-01] MCP `args`/`command` not redacted server-side — `npx ... --api-key <tok>` returned cleartext + painted in mcp-panel + config-panel. `config_api.rs:133` array branch; `console.ts:360` masks only env.
- **A6** [UI-02] MCP `env` redaction is client-side only (`console.ts:351`), backend suffix list misses operator-named vars (`DATABASE_URL`, `PGPASSWORD`, `*_DSN`). Move to server, blank all env values.
- **A7** [UI-03] `proxy.http_proxy/https_proxy/all_proxy` returned + rendered unredacted (userinfo `user:pass@`), also logged verbatim at `schema.rs:1566`. Redact userinfo only.
- **A8** [SEC-05] `auth-profiles.json` (OAuth access+refresh tokens) written with no 0600; world-readable under system install `/etc/rantaiclaw`. `profiles.rs:400`; OpenRC installer doesn't chmod it.
- **A9** [SEC-09] `config.toml` temp file gets 0600 AFTER write, not at open — brief world-readable window on every save/PUT. `schema.rs:4598`. Mirror `secrets.rs:186` `.mode(0o600)` at open.
- **A10** [SEC-11] Lark `verification_token` prompt has `secret: false` (only such credential prompt in the tree) — echoed in TUI/stdout. `channels/lark.rs:204`.

## B. Security — privileged write path / exposure (HIGH)

- **B1 👑✓** [SEC-02] `POST /api/v1/config/mcp_servers/{name}` = ungated persistent subprocess exec. Only checks name+command non-empty, then spawns `Command::new(command)` at agent build. `config_api.rs:440-458`; caller-supplied env overrides hardened env. Chat pairing token → local RCE, no approval/audit/cap.
- **B2** [UI-05] Same route is silent full-replace: re-adding a name wipes its `env`+`args` (panel `add()` never sends env). `config_api.rs:449`; `mcp-panel.tsx:42`. Merge, don't replace; 409 on collision. Rotate wiped keys.
- **B3** [SEC-07] `require_pairing` not hot-reloaded — reloader syncs token list but never rebuilds the guard flag. Incident lockdown (`require_pairing=true`) ineffective until restart, while token revoke DOES reload → looks like it works. `gateway/mod.rs:1267`; `pairing.rs:35`.
- **B4** [DEBT-05 = BUG-CFG-13 = UI-08] Gateway write path doesn't run `Config::validate()` before persist. `PUT autonomy {max_actions_per_hour:0}` persists → daemon then refuses to start (bricks). Temperature out-of-range accepted from console, rejected from CLI. `config_api.rs:299/383`; `schema.rs:4094/4130`.
- **B5** [SEC-12] `always_ask`/`auto_approve` entries unvalidated on PUT (neighbouring `allowed_commands` IS validated). Typo → exact-match never fires → silent fail-open, echoed back as if enforced. `config_api.rs:363`; `approval/mod.rs:112`.
- **B6** [SEC-08] Audit subsystem (`AuditEventType::ConfigChange` etc.) has ZERO callers. Policy-weakening writes (autonomy→Full, empty always_ask, block_high_risk_commands=false) leave no record. `security/audit.rs:13`.
- **B7** [SEC-10] Installer `chown -R` lacks `--no-dereference`; on BusyBox/Alpine follows symlinks → local priv-esc primitive. `service/mod.rs:808`. MED.

## C. Correctness — config data loss / corruption (HIGH)

- **C1 👑✓** [BUG-OB-02] `setup channels` re-run whole-struct-replaces `channels_config` from default → wipes `approval_owners`, Telegram token, guest ceilings. All tool calls auto-deny after. `section/channels.rs:39`; `wizard.rs:3424`. Also `run_channels_repair_wizard` (485). TUI path is safe (per-field). 
- **C2 👑✓** [BUG-CFG-01] Env-var overrides get serialized into `config.toml` on any console write (`load_or_init` overlays env → `save()` writes the overlaid struct). PORT/API_KEY/HTTP_PROXY burned to disk, then shadow the env they came from (config outranks env). `schema.rs:4055/4493`; `config_api.rs:241`.
- **C3** [BUG-OB-03] `rantaiclaw setup` does `load_or_init().unwrap_or_default()` then `save()` → any load error erases the real config to defaults. Also `save()` never validates → can persist a config the loader then refuses (circular lockout; the recovery command dies the same way). `main.rs:1635/1655`.
- **C4** [BUG-CFG-06 = BUG-LC-10] Migration write-back is bare `fs::write` (truncate, no temp/backup) — the one write touching every config on every upgrade; crash/ENOSPC mid-write loses all secrets, one-way, no `.bak`. `schema.rs:4018` vs atomic `save()` 4596.
- **C5** [BUG-OB-17 = SEC-04] OpenClaw migration hand-rolls TOML with no escaping → invalid file on any non-trivial slug/key, injectable (`require_pairing`, `allowed_users=["*"]`) from a hostile `openclaw.json`, writes plaintext skill credential bypassing encryption, file under umask not 0600. Reports success anyway (error swallowed to `warn!`). `openclaw.rs:350-414`.
- **C6** [BUG-OB-05] Gateway provisioner: "disable" answer writes nothing (no `enabled` field exists); choosing `0.0.0.0` writes host but not `allow_public_bind` → gateway then refuses to start. `runtime_surfaces/gateway.rs:68/174`.
- **C7** [BUG-OB-06] Tunnel provisioner whole-replaces `TunnelConfig`; selecting a provider with empty token persists `provider="cloudflare", cloudflare:None` (impossible state) + wipes stored token/domain. `tunnel.rs:91/284`.
- **C8** [BUG-OB-08] Browser provisioner prompts viewport w/h/quality then discards all three (`let _w`), hardcodes defaults, wipes `allowed_domains` → breaks browser tool. `browser.rs:159/188`.
- **C9** [BUG-CFG-07] Decrypt-path drift after #567: `channels/routing.rs:332` `load_runtime_defaults_from_config_file` decrypts only `api_key`, skips migrate+validate; `gateway_agents.*.api_key` in neither encrypt nor decrypt list (plaintext at rest). 
- **C10** [BUG-CFG-16] Out-of-band writers (`persist_approval_owner`, `persist_pairing_tokens`) bypass `CONFIG_WRITE_LOCK` and save the whole file from a stale in-memory snapshot → clobber concurrent edits. `config_api.rs:228`; `gateway/mod.rs:1288`.

## D. Correctness — lifecycle broken (HIGH unless noted)

- **D1 👑✓** [BUG-LC-01] launchd plist is a raw string with `\"` escapes that land verbatim → invalid XML → `launchctl load` rejects → macOS service install never works. `service/mod.rs:539-542`.
- **D2 ✓** [BUG-LC-03] `--host localhost` (and `::1`, `0:0:...:1`) crashes gateway: `format!("{host}:{port}").parse::<SocketAddr>()` needs numeric IP, but `is_public_bind` whitelists these spellings. `gateway/mod.rs:911`; `pairing.rs:367`.
- **D3** [BUG-LC-04] Daemon supervisor restarts on ANY Err forever (bind refusal, bad host, EADDRINUSE are fatal) while printing "daemon started" → silent infinite loop, systemd sees healthy. `daemon/mod.rs:270-295`; `gateway/mod.rs:902`.
- **D4** [BUG-LC-05] `build_gateway_router` unconditionally spawns `spawn_config_reloader` (inotify + task, no cancel token) → leaked per gateway restart; exhausts `max_user_watches` in a crash loop; also races the shared config write. `gateway/mod.rs:812/1253`.
- **D5** [BUG-LC-02] Profile/unit identity never written into the installed unit → service always runs profile `default`; `profile use` restarts `rantaiclaw@<name>.service` which the installer never creates → handoff can't work. `service/mod.rs:597`; `daemon/handoff.rs:152`.
- **D6** [BUG-LC-16] `rantaiclaw gateway` passes a `CancellationToken::new()` nothing cancels → `with_graceful_shutdown` never fires; SIGTERM severs in-flight requests. `main.rs:1866`.
- **D7** [BUG-OB-14] Auth lock file `create_new` with no stale detection → one crash/SIGKILL orphans it → every auth op blocks 10s then fails forever, no recovery hint. `profiles.rs:454-521`.
- **D8** [BUG-LC-08] Blocking service-control calls (`systemctl restart`, up to 30s) run inside async fns at 4 sites; worst is `whatsapp_web.rs:251` freezing the TUI runtime. Pattern already fixed at `config_api.rs:566`. `main.rs:2056`; `channels/admin.rs:59`.
- **D9** [BUG-LC-06] Daemon shutdown hard-`abort()`s channels (discards the cancellation token it was handed) → dropped replies, uncommitted long-poll offsets (dup reprocessing). `start_channels_with_cancellation` exists + is used by TUI. `daemon/mod.rs:95/169`.
- **D10** [BUG-LC-07] `restart_daemon_for_profile_with` restarts unconditionally without `pid_is_alive`/`is_active` check → `profile use` STARTS a daemon the operator had stopped after an unclean exit. `handoff.rs:172`; `is_active` has zero prod callers.
- **D11** [BUG-LC-13] Supervisor backoff sleep not raced against shutdown token → SIGTERM during backoff ignored until sleep elapses (every stop takes the full 8s drain when gateway is retrying). `daemon/mod.rs:270-295`.
- **D12** [BUG-LC-09] `daemon_state.json` written non-atomically every 5s; readers (`doctor`, TUI) treat a torn read as a hard error → intermittent false "daemon broken". `daemon/mod.rs:239`. MED.
- **D13** [BUG-LC-11] Generated systemd unit interpolates `ExecStart`/`WorkingDirectory` unquoted + doesn't escape `%` → a path with a space or `%` produces a unit that fails to start after "install succeeded". `service/mod.rs:609`. MED.
- **D14** [BUG-LC-12] macOS `maybe_restart_managed_daemon_service` does async `launchctl stop`+`start` (race) and reports `Ok(true)` on start's exit code → tells operator "reloaded" while daemon may be dead/old-config. `channels/admin.rs:203`. Use `kickstart -k`. MED.
- **D15** [BUG-LC-14] `linux_service_file` hard-codes `~/.config/systemd/user/...`, ignores `XDG_CONFIG_HOME` → install writes where systemd can't see it; "Unit not found" after success. `service/mod.rs:1232`; dup path in `admin.rs:240`. MED.
- **D16** [BUG-LC-15] `run_capture` never checks exit status → Windows `is_service_installed` returns true whenever `schtasks.exe` exists (latent; non-Linux early-returns today). `service/mod.rs:1253/143`. 
- **D17** [BUG-LC-17] `pid_is_alive` = `kill(pid,0)` answers "some process", not "the daemon" → after SIGKILL + PID reuse the TUI permanently refuses to start channels. `profile/sentinel.rs:95`; TUI sole gate `app.rs:2330`. MED.

## E. Correctness — setup / doctor honesty (HIGH)

- **E1** [BUG-OB-04] `setup approvals --force` hardcodes `force=false` → preset never changes on an existing install; "Approval policy set: Strict" printed while runtime keeps old preset. No in-product way to change it. `section/approvals.rs:52`; `provision/approvals.rs:115`.
- **E2** [BUG-OB-07] Anthropic key validation + `provider.ping` use `Bearer` (Anthropic needs `x-api-key` + `anthropic-version`) → valid keys reported rejected; both push operator to replace a working credential. `provision/provider.rs:382`; `checks/provider.rs:103`.
- **E3** [BUG-OB-09] CLI `doctor` calls `run_all` (drops the `skipped` list #624 added) → `--brief` prints "7/7 ok" on a run that never probed provider/channels/mcp. `main.rs:2103`.
- **E4** [BUG-OB-11] `system.deps` uses `spawn_blocking(...).unwrap_or_default()` → on JoinError reports all binaries present (vacuous green). Also `sha256sum` recommended but is `shasum` on macOS. `checks/system_deps.rs:23`.
- **E5** [BUG-OB-12] Setup uses `env::var(...).is_ok()` → an exported-but-empty key reads as "detected", skips the prompt, finishes with an unusable config. Correct idiom exists 160 lines later. `wizard.rs:2598`.
- **E6** [BUG-OB-10] Online `channels.auth` returns "no channels configured" for an incomplete WhatsApp block that offline `inspect_channels` correctly fails → the two doctor modes contradict. `checks/channels.rs:175`.
- **E7** [BUG-OB-16] `setup`/`doctor` disagree on "has a key": `ProviderSection` checks only top-level `api_key`, writes only there → a console-configured key (`provider_api_keys`) reads absent → re-prompts every run + duplicates. `section/provider.rs:33`; `legacy.rs:353` still `Some("")`-is-ok.
- **E8** [BUG-OB-13] TUI login provisioner says "left disabled" on empty/mismatched password but clears nothing → old password gate stays armed. Three surfaces, three behaviours. `provision/login.rs:115`.
- **E9** [BUG-OB-15] OAuth loopback accepts exactly one connection, returns the request path as the "code" when no `?`, and SKIPS state verification on that path → a stray preconnect/favicon fetch derails the flow + defeats CSRF binding. `openai_oauth.rs:242-329`.
- **E10** [BUG-OB-18] First-run wizard `is_channel_name` uses a hardcoded 16-entry array while the picker is registry-driven → the next channel added without editing the array loops the user back to the picker with no way forward. `first_run_wizard.rs:86/203`. MED.
- **E11** [BUG-OB-19] Nothing validates `model_routes[].provider` (provisioner, `validate()`, or doctor) → a typo'd provider is accepted everywhere, fails at routing time. No dup-hint check either. `model_routes.rs:124`; `checks/config.rs:23`.

## F. Correctness — config core (HIGH)

- **F1** [BUG-CFG-02] Proxy env feedback loop: `apply_env_overrides` sets proxy env when enabled with no clear-on-disable branch, then re-reads `HTTP_PROXY` it wrote itself → resurrects a disabled proxy; combined with C2 rewrites `enabled=true` to disk. Also `set_var` from reload task races worker `env::var`. `schema.rs:4486/4434`. Fix pattern at `tools/proxy_config.rs:254`.
- **F2** [BUG-CFG-03] 3-way default drift serde vs `impl Default` vs docs: `HttpRequestConfig` (all 4 fields), `BrowserConfig.enabled`, `WebSearchConfig.enabled`, `block_high_risk_commands`, `CostConfig.prices`. A hand-added `[http_request]` section gets `allowed_domains=[]` → rejects every request. `schema.rs:1262/1284`.
- **F3** [BUG-CFG-04] `[autonomy]` (+ `default_temperature`, `channels.cli`, `memory.backend`, `heartbeat.*`, `observability.backend`) have no serde defaults → a partial section (`[autonomy]\nlevel="full"`, exactly what docs teach) fails the whole load with "missing field". `schema.rs:2184`.
- **F4** [BUG-CFG-05] Schema-drift gate fingerprints `schema_for!(Config)` (serde side) only, not `Config::default()` → CLAUDE.md's "defaults are fingerprinted" is false for the values a fresh install actually gets; every F2 drift shipped green. `tests/schema_drift.rs:31`.
- **F5** [BUG-CFG-09] Ad-hoc env parsing: `allow_public_bind`/`web_search.enabled` assigned unconditionally from `val=="1"||"true"` → `WEB_SEARCH_ENABLED=yes` DISABLES it; invalid PORT/temperature/timeout silently discarded with no warn. Strict parser `parse_proxy_enabled` exists but used once. `schema.rs:4326/4354/4310`.
- **F6** [BUG-CFG-10] `RANTAICLAW_CONFIG_DIR` + `RANTAICLAW_WORKSPACE` split-brain: config_path from one, workspace_dir from the other → skills/memory/workspace-policy resolve against a different tree than the config. `schema.rs:3676/4264`.
- **F7** [BUG-CFG-12] Malformed `schema_version` (string `"23"`, float, negative) → `as_integer()` None → silently v0 → re-runs whole chain + rewrites file every load; negative wraps to a bogus "update rantaiclaw" error. `migrations.rs:49`.
- **F8** [BUG-CFG-08] `migrate_v18` reads `KB_EMBEDDING_API_KEY` from process env as migration evidence → outcome depends on which process ran first; once stamped, KB stays off permanently with the key present. `migrations.rs:366`.
- **F9** [BUG-CFG-13] File-supplied values skip the range checks the env path applies (`default_temperature`, `web_search.max_results`, timeouts) → `temperature=99.0` loads and 400s every request. (Overlaps B4.) `schema.rs:4331/4095`.
- **F10** [BUG-CFG-14] TUI `/doctor` runs a full `Config::load_or_init()` (migrations + possible file write + env-override + proxy-env mutation, blocking the render thread) just to resolve a directory path. `resolve_active_paths()` exists. `tui/commands/config.rs:275`.
- **F11** [BUG-CFG-15] `/config <key> <value>` usage + panel advertise persistence; implementation is a 2-key session toggle (`model`, `debug`) that never touches `Config`/`save()`. `debug` silently coerces non-`true` to false. `tui/commands/config.rs:72/120`.

## G. Tech debt / dead code (HIGH unless noted)

- **G1** [DEBT-06 = BUG-OB-20] `doctor/legacy.rs` `run` + `check_config_semantics` (~630 of 1274 lines) unreachable, kept alive by their own ~20 tests; the live twin `provider_validation_error` regressed the fix the dead one documents (`None` vs `Some("doctor-shape-check")`). `doctor/mod.rs:16`; `checks/config.rs:224`.
- **G2** [BUG-CFG-11] `src/config/runtime.rs` never declared in `config/mod.rs` → 158 lines + 5 never-run tests compile nowhere, and its header documents a `config.runtime.toml` overlay feature the binary doesn't implement. `config/mod.rs:1`.
- **G3** [DEBT-02] Two parallel onboarding frameworks: `SetupSection` (8 impls) vs `TuiProvisioner` (7 of the same names), fully independent, `setup <topic>` tries provisioner first. 7 topics implemented twice → drift (one already found). `section/mod.rs` vs `provision/traits.rs`.
- **G4** [DEBT-03] Three hardcoded orderings of one onboarding journey: `canonical_sections()`, `REQUIRED_PROVISIONERS`+`INTEGRATION_OPTIONS`, `registry::available()`. `knowledge` missing from first-run TUI; `memory`/`web-search` missing from headless. `wizard.rs:127`; `first_run_wizard.rs:64`; `registry.rs:97`.
- **G5** [DEBT-04] Provider→env-var table in 3 copies, 2 already wrong: `providers/mod.rs:887` (authoritative), `wizard.rs:2960`, `app.rs:5007` (vercel/zai/ollama/venice/... drifted) → TUI "Missing API key" hint names a var the runtime doesn't read.
- **G6** [DEBT-01] `finalize_channel` (shared post-provisioner hook) doesn't reload the daemon → channel configured via TUI setup/`--non-interactive` appears set but the running daemon keeps the old channel set. Web console + `channel bind-telegram` reload correctly. `provision/mod.rs:78`.
- **G7** [DEBT-07 = DOCS-03] Dead config keys serialized + documented as functional: `security.resources.*`, `audit.sign_events`, `cost.allow_override`, `cost.prices` (+80-line default table), `agent.parallel_tools`, `sandbox.firejail_args`. `security/mod.rs:11` already says the sandbox/audit blocks "have no effect today". `schema.rs:3282/3344/720/406/3248`.
- **G8** [DEBT-08] `schema.rs` (7594) + `wizard.rs` (7219) god files, ~29× median, ~4 commits/month each = merge-conflict funnel; clean seams exist (channel structs, `curated_models_for_provider`, `setup_channels`). Assessment, not a rewrite. Sequence right after a release.

## H. Test coverage / quality (HIGH unless noted)

- **H1** [TEST-01] 9 of 11 `config_api` routes have no 401 test (incl `set_secrets`, `add_mcp_server`); deleting `check_auth` from them lands green. `require_pairing==false` bypass branch also untested. `tests/config_api.rs:8`.
- **H2** [TEST-02] 12 `runtime_surfaces` provisioners (~2300 lines) have no test/seam; `memory.rs` + `runtime.rs` already exhibit the whole-struct-replace bug the gateway test guards. `runtime_surfaces/*.rs`.
- **H3** [TEST-07] Migration version gate self-referential — 27/28 tests assert against `CURRENT_VERSION`; a bump with no arm + no test + `insta accept` ships green. `migrations.rs:36`; `schema_drift.rs:40`.
- **H4** [TEST-05] `pkce_generation_is_valid` never asserts `challenge == SHA256(verifier)` (the whole PKCE property) or that two calls differ; exchange/refresh/device-code untested. `openai_oauth.rs:459`.
- **H5** [TEST-10] `LoginSection::run` (console-password gate) is a straight dialoguer script with no seam → untested; inverting `p1==p2` ships green. `section/login.rs:41`.
- **H6** [TEST-12] `redact_secrets_in_json` completeness "guarantee" tested against a hand-written 23-key fixture, not `serde_json::to_value(&Config)` → a new non-suffixed secret field leaks + the test can't see it. `config_api.rs:1146`.
- **H7** [TEST-13] `service/mod.rs`: 25 tests, none on install/uninstall/start/stop; `uninstall_linux` removes files from a computed path with no test. `service/mod.rs:1275`.
- **H8** [TEST-09] ~30 HOME/env-mutating tests restore via trailing statements (leak on panic); CI runs `--test-threads=1`, pre-push doesn't → local flake CI can't see, and `ENV_LOCK` untested in CI. `HomeGuard` exists, used twice. `schema.rs:6740`; `ci-run.yml:134`.
- **H9** [TEST-03] `recording_stub_counts_calls` tests the mock, never calls `restart_daemon_for_profile_with`; `fail_with` never exercised → daemon-restart path unverified. `handoff.rs:286`.
- **H10** [TEST-04] `setup_propagates_section_failures_and_stops` injects no failing section; sole assert `!err.is_empty()`; duplicate of another test. `tests/setup_orchestration.rs:188`.
- **H11** [TEST-06] `atomic_write_replaces_file` writes once, asserts existence — proves neither atomicity nor replacement nor tmp cleanup nor mode. `profiles.rs:688`.
- **H12** [TEST-08] `load_or_init` migration + credential-strip write-back has zero coverage — deleting the write-back block ships green (migration re-runs; plaintext `api_url` credential never removed from disk). `schema.rs:3998`.
- **H13** [TEST-11] `runtime_and_hardware_still_resolve...` asserts two resolvers independently return Some, never calls `SetupCommand::execute`; the documented ordering can invert green. `tui/commands/setup.rs:216`.
- **H14** [TEST-14] Config watcher's Access-event filter + debounce drain both unfalsifiable (sibling test discriminates on filename, not event kind); the reload-loop regression can return green. `watcher.rs:52`.
- **H15** [TEST-15] 7 near-identical vacuous `!description().is_empty()` section tests; TUI `DoctorCommand` (documented past false-green) untested. `section/*.rs`; `config.rs:146`.
- **H16** [TEST-16] Flake patterns: `MOCKITO_LOCK.lock().unwrap()` poisons cascade; a real 5s-timer MCP test under `--test-threads=1`; `tests/config_persistence.rs` 9 default-value asserts triple-covered + churn on every default change. MED.
- **H17** [TEST-17] `first_run_wizard` tests only back()/scroll; the forward state machine (`start_provisioners`, `advance_to_next_in_queue`, `picker_submit`) untested → a dropped/skipped provisioner ships green. `first_run_wizard.rs:1226`.

## I. Performance (HIGH unless noted)

- **I1** [PERF-01] All-provider model catalog probe is a serial `for` loop, 8s timeout each → `doctor models`/`models refresh --all` worst case ~34×8s ≈ 4.5min behind a firewall. `join_all` pattern exists in same subsystem. `legacy.rs:169`.
- **I2** [PERF-02] Gateway pays two full config loads per own write (handler `load_or_init` + watcher reload on the file it just wrote); no content/fingerprint gate; also mutates process-global proxy env from the reload task. `config_api.rs:244`; `gateway/mod.rs:1267`; `fingerprint.rs` exists.
- **I3** [PERF-03] `load_or_create_key` re-reads `.secret_key` (blocking `std::fs`) once per secret field → ~10-60 reads per config load/save; no memoization; runs on async worker. `secrets.rs:171`.

## J. Docs (HIGH — cheap, runtime-contract accuracy)

- **J1** [DOCS-01] `config.toml` path wrong in 3 docs — default resolves to the PROFILE dir, not `~/.rantaiclaw/config.toml`. Breaks the documented token-revocation procedure (edits a file the runtime never loads). `config.md:7`; `runbook.md:114`; `troubleshooting.md:181`.
- **J2** [DOCS-02] Feature-gating notes inverted — `kb` ships ON (documented as gated/off), `hardware` ships OFF (documented ungated). `commands.md:30/28` vs `Cargo.toml:264/276`.
- **J3** [DOCS-04] one-click-bootstrap leads with unpublished `brew install` (fails "No formula") + inverts the default (setup --force runs by default; `--skip-setup` undocumented). `one-click-bootstrap.md:7/100`.
- **J4** [DOCS-05] troubleshooting low-RAM remedy recommends deprecated no-op `--prefer-prebuilt` and `--features hardware` (makes the OOM build HEAVIER). `troubleshooting.md:48/69`.
- **J5** [DOCS-08] `commands.md` top table omits ~1/3 of the CLI incl the lifecycle (`update`/`rollback`/`uninstall`/`auth`/`profile`/...); `doctor` flags + `migrate --from zeroclaw` undocumented. `commands.md:9` vs `main.rs`.
- **J6** [DOCS-06] Setup section catalog stale in BOTH `commands.md:62` and the binary's `setup --help` (`main.rs:257`) — missing `approvals` + `login` (the security-relevant ones).
- **J7** [DOCS-07] `config.md` self-contradicts on `message_timeout_secs` (table says 600, note says 300). `config.md:622/645`; real default 600.
- **J8** [UI-16] Stale doc comment claims Telegram bot tokens stored plaintext; they're encrypted (`schema.rs:4547`). Decision drift. Note: Discord/Slack/Mattermost tokens genuinely NOT wrapped. `config_api.rs:617`.

## K. DX / tooling (HIGH)

- **K1** [DX-01] Nothing verifies a documented default/flag/path against code; the docs-quality CI job runs only on `docs_changed`, so a code change that invalidates a doc claim triggers no gate. Every J-finding is that shape. `ci-run.yml:270`.
- **K2** [DX-02] No `deny_unknown_fields` anywhere (60 structs) → a mistyped key silently no-ops; parse errors go through `toml::Value` losing spans and name neither file nor key. `schema.rs:3998`.
- **K3** [DX-03] `.env.example` omits every `KB_*` var (KB is env-only config, default-on) + `RANTAICLAW_CONFIG_DIR` (resolution step 1) + skills/UI/skip-setup vars. `.env.example`.

## Direction (options for the maintainer, not bugs)

- **DIR-01** Generalize the unread-config-key CI gate from 15 channel structs to the whole schema — mechanism + CI wiring + fail-closed policy already exist; `parallel_tools` (G7) sits in the blind spot. `check_channel_config_readers.sh`.
- **DIR-02** Close the config read/write asymmetry — gateway reads all, writes 6 things; no `config set`/`config validate`. Schema (`schema_for!(Config)`) already emitted. Design spike; land `config validate <file>` first (no security surface).
- **DIR-03** Retire `src/peripherals` + `hardware` — code is feature-gated + default-inert, but CLI declarations appear in default `--help`, config structs are frozen in the schema snapshot, and docs promise `[hardware]`/`[peripherals]` as ordinary config. Owner has declared it unused. Cheap half: gate the CLI decls + fix docs now; schema removal rides the next `CURRENT_VERSION` bump.
- **DIR-04** Resolve the half-built setup section catalog — `section/mod.rs:46` names 8 sections ("workspace, tunnel, tools, hardware, memory, project_context, workspace_files, daemon") never built; `daemon` + `memory` have demonstrated demand in troubleshooting docs. Reconcile against the spec or prune.

## L. claw-ui Configuration surface — UX & robustness (repo: claw-ui) (HIGH unless noted)

- **L1** [UI-06] Config panel's temperature card renders unconditionally — no `cfg.loading`/`cfg.error` guard, Save always enabled → on a failed `GET /config` the operator writes against a config they never read. `config-panel.tsx:42`.
- **L2** [UI-07] Config/tools/providers panels pass `loading`/`error` to `PanelFrame` but omit the `loaded` prop → a post-save refresh 502 blanks the whole panel, so a SUCCESSFUL save presents as a load error. Highest-frequency in Tools & Autonomy (every toggle refreshes). `config-panel.tsx:73`; `tools-panel.tsx:101`; `providers-panel.tsx:151`.
- **L3** [UI-09] Config panel (temperature) + MCP nav badge have the stale-snapshot class `SKILLS_CHANGED`/`PERSONA_CHANGED` were built to fix — load-time `useState`, never re-read → right rail shows the pre-edit temperature all session; MCP badge stays wrong. `console-shell.tsx:374`.
- **L4** [UI-10] Console can't clear `api_key`/`api_url` — the backend's clear-on-empty contract is unreachable because the panel guards `if (key.trim()||url.trim())` and sends `x.trim()||undefined` → a compromised key can't be revoked from the console. `providers-panel.tsx:56`; backend contract `config_api.rs:794`.
- **L5** [UI-11] Provider save is two non-atomic writes (`setConfigModel` then `setSecrets`) with the refresh calls inside the `try` after both awaits → if the second fails, the gateway has switched provider while the console still shows the old one and re-runs nothing. `providers-panel.tsx:49`.
- **L6** [UI-12] `describeApiError` (built to distinguish 401 "log in again" from 502 "wait" from 400 "bad input") is bypassed by 5 config panels + by `useAsync` (which stores `e.message`) → an idle-timeout 401 shows as a bare "unauthorized" toast, operator retries instead of re-authenticating. `api.ts:52`; `use-async.ts:29`.
- **L7** [UI-13] Internal error detail (absolute `config.toml` path, gateway host:port) relayed verbatim to the browser via `err_500` → BFF byte-for-byte → rendered raw in toast/hint. `config_api.rs:85`; `route.ts:33`.
- **L8** [UI-14] `api.config()` is untyped `Record<string,unknown>`, hand-cast at 5 consumers → a Rust-side rename compiles clean both sides and degrades silently at runtime (temperature blank, autonomy falls back to "smart", MCP badge 0). `api.ts:327`. `schema_for!(Config)` already derived → generate the interface.
- **L9** [UI-15] Unsaved edits discarded on route switch with no warning — `PANELS[route]` swaps element type, unmounting panel state; worst in Providers where a pasted-but-unsaved key vanishes on a rail click. `ops-view.tsx:19`. MED.
- **L10** [UI-17] Autonomy rung buttons send partial deltas (`strict`/`off` omit `always_ask`) while `set_autonomy` applies only present fields → `always_ask` residue persists under a `full` level; enforcement is safe (short-circuits on Full) but the Tools panel + `preset_for_autonomy` read a stale discriminator. `console.ts:318`; `config_api.rs:359`. MED.

---

## Cross-agent merges (become ONE plan each when planned)

- A1 (key gating) + A2 (gemini URL redaction): same command surface, likely one plan.
- A4 = SEC-06 = UI-04 (api_url/mcp-env redaction) + A5/A6/A7 (mcp args, mcp env, proxy userinfo): redaction cluster.
- B1 (RCE gate) + B2 (env-wipe merge): same route `POST mcp_servers`.
- B4 = DEBT-05 = BUG-CFG-13 = UI-08 (validate-before-persist; temperature/max_actions range): one validation plan + a UI input-range piece.
- C5 = BUG-OB-17 + SEC-04 (OpenClaw migration): security superset.
- G1 = DEBT-06 = BUG-OB-20 (legacy doctor).
- G7 = DEBT-07 + DOCS-03 (dead keys + their docs).
- C6/C7/C8 (gateway/tunnel/browser provisioner whole-replace) + C1 + H2: the "provisioner overwrites unprompted config" family — could be one hardening plan across all runtime_surfaces or split per surface.

## Not-a-finding (recorded so they aren't re-audited)

- `forbidden_paths` wholesale accept — floor is unioned in `policy.rs:994`, regression-tested; by-design.
- Credential precedence config-over-env, default-on local tools, arrayref `=0.3.9` pin, per-principal rate limit (#638), per-turn approval scoping (#640) — settled.
- OAuth PKCE mechanics sound (S256, OsRng verifier, state compared, loopback 127.0.0.1) — only the loopback single-accept (E9) and the missing test (H4) are findings.
- Tunnel child reaped via `kill_on_drop(true)`; doctor checks already `join_all`-parallel; `fingerprint.rs` "none" sentinel deliberate; `api_url.rs` keep-malformed/drop-credential split test-pinned.
- Minor cosmetic (own note, not planned unless bundled): default-model slug hyphen vs dot `claude-sonnet-4-6` in docs vs `4.6` in code (`schema.rs:83` vs `3418`) — doc copy-paste 404s.
