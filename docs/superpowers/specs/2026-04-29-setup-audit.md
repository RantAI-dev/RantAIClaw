# Setup-flow audit — what actually works, what's a stub

**Date**: 2026-04-29
**Trigger**: User reported `rantaiclaw` setup → WhatsApp Web → "QR" flow does not show a QR code.
**Scope**: `rantaiclaw setup` interactive wizard + `rantaiclaw onboard` quick mode + `rantaiclaw doctor` follow-ups, focused on the channel and integration paths that promise to "validate" something.

## TL;DR

| Setup path                | Validates during setup? | Notes                                                                                                          |
| ------------------------- | ----------------------- | -------------------------------------------------------------------------------------------------------------- |
| **Provider** (LLM)        | No live ping            | Tier picker is comprehensive; encrypted api_key has a separate migration bug (#39).                           |
| Telegram                  | ✓ `getMe`               | Real round-trip; reports bot username on success.                                                              |
| Discord                   | ✓ `users/@me`           | Real round-trip; reports bot username on success.                                                              |
| Slack                     | ✓ `auth.test`           | Real round-trip; reports workspace name + Slack-side error code on failure.                                    |
| **WhatsApp Web (QR)**     | **✗ broken**            | **No QR ever surfaces during setup**; only writes config. See §1.                                              |
| WhatsApp Cloud API        | ✓ phone-number probe    | Real round-trip; resolves phone number id.                                                                     |
| MCP servers — authed      | ✓ spawn + JSON-RPC ack  | `validate_mcp_startup` boots the binary and waits ≤5s for any stdout response; skip + warn on failure.         |
| MCP servers — zero-auth   | ✗ skipped               | `register_mcp` runs without a `validate_mcp_startup` round-trip — silent if the install_command isn't on PATH. |
| Approvals (L1-L4)         | n/a (file-write only)   | Writes preset TOML files; no runtime cross-check that the gate loads them.                                     |
| Persona                   | n/a (template render)   | Pure file write; works.                                                                                        |
| Skills (starter pack)     | ✓ idempotent install    | Copies bundled SKILL.md into `<profile>/skills/`. Skips if dir exists.                                         |
| Skills (ClawHub browse)   | ✓ HTTP fetch + cache    | Real `GET /api/v1/skills?sort=stars`; 24h cache.                                                               |
| Doctor (`channels` check) | **misleading**          | Reports `live` category but only checks that bot_token strings are non-empty. No WhatsApp coverage. See §6.    |
| Doctor (`provider.ping`)  | ✓ optional              | Mockito-tested round-trip; opt-in via `--brief`/`--offline` flags.                                             |

## 1 · WhatsApp Web QR flow — `BROKEN`

**File**: `src/onboard/wizard.rs` lines 3629-3712 + `src/channels/whatsapp_web.rs` line 335-340.

**What the wizard does**: prompts for `session_path`, `pair_phone` (optional), `pair_code` (optional), and an allowlist. Then writes the config and prints "scan QR in WhatsApp > Linked Devices" as a hint. **It does not start the WhatsApp Web client.**

**What the WhatsApp Web client does** when the daemon eventually starts it:

```rust
Event::PairingQrCode { code, .. } => {
    tracing::info!(
        "WhatsApp Web QR code received (scan with WhatsApp > Linked Devices)"
    );
    tracing::debug!("QR code: {}", code);   // ← QR string only at DEBUG level
}
```

The actual QR payload is logged at **DEBUG**, which is hidden in default tracing setups. Even if a user gets to the daemon-runs phase, the QR is invisible. And the QR is a raw base64 string, not a scannable bitmap.

### Two failures stacked

1. **Setup wizard never spawns the client.** It cannot — that's a daemon-time concern. So during `rantaiclaw setup channels`, no QR shows up; the user finishes onboarding without scanning anything.
2. **When the daemon does run the client, the QR is printed wrong.** Logged at DEBUG, no terminal-rendered bitmap, no human-readable instruction.

### Fix sketch

* Add a setup-time fast path: if `session_path` does not yet exist and `pair_phone` is empty, **launch a one-shot in-process `wa-rs` client** during the wizard, render the QR with the [`qrcode`](https://crates.io/crates/qrcode) crate (`as_terminal_render()`-style), wait for `Event::Connected`, then write the config and exit cleanly.
* In the long-running daemon path: change `tracing::debug!("QR code: {}", code)` to **render the QR to stderr unconditionally** (with a clear "Scan with WhatsApp → Linked Devices → Link a Device" header). Same `qrcode` crate.
* Same for `Event::PairingCode { code, .. }` — print the human-readable pair code instead of relying on `tracing::info!` filtering.

### Build-feature reminder

The whole path requires `--features whatsapp-web`. The wizard prints "1. Build with --features whatsapp-web" but only as a hint — there's no compile-time check or runtime warning if the user runs `cargo build --release` without the feature. Add a runtime branch that prints a clear error when `WhatsApp Web` is selected and the feature isn't compiled in.

## 2 · MCP zero-auth installs — silent if binary missing

**File**: `src/mcp/setup.rs` lines 50-55.

```rust
if install_zero_auth {
    for server in NO_AUTH {
        register_mcp(config, server, &[])?;     // writes config block
        info!("MCP server registered: {}", server.slug);
    }
}
```

Authed servers go through `collect_and_register` → `validate_mcp_startup` (spawn + 5s `initialize` round-trip). Zero-auth servers skip that. If `npx @modelcontextprotocol/server-filesystem` (or whichever curated install_command) isn't on PATH, the user only finds out at agent-launch time when the tool registry fails to enumerate the server.

### Fix

Call `validate_mcp_startup(server, &[])` for the zero-auth loop too. Same skip+warn semantics: an offline machine simply doesn't get those servers added, but the user sees the failure during onboarding.

## 3 · Approvals — preset write but no runtime cross-check

**File**: `src/onboard/section/approvals.rs`.

The L1-L4 picker writes `<profile>/policy/{autonomy,command_allowlist,forbidden_paths}.toml` from bundled presets and confirms with a green check. There's no runtime probe that the approval gate actually loads these files (`src/approval/policy_writer.rs` writes them; `src/approval/allowlist.rs` reads them). A typo or schema drift between the presets and the loader would only surface at first tool call.

### Fix (low priority)

After writing the files, re-instantiate `ApprovalGate::from_profile(...)` and assert it loads without error. Cheap, catches schema regressions during PR review.

## 4 · Provider — encrypted api_key migration bug (already filed)

Already covered by **PR #39**: `fix(profile): migrate .secret_key + secrets/ alongside config.toml`. v0.4.x → v0.5.0 migration left the secret key behind, so the encrypted `api_key` blob refused to decrypt. End-to-end smoke test workaround is `OPENROUTER_API_KEY=...` env var. Not regressed by this audit; flagged here for completeness.

## 5 · Doctor — `channels.auth` is misleading

**File**: `src/doctor/checks/channels.rs`.

```rust
fn category(&self) -> &'static str { "live" }
async fn run(&self, ctx: &DoctorContext) -> CheckResult {
    let summary = inspect_channels(&ctx.config);   // ← config-only
    ...
}
```

The check is bucketed into the `live` category alongside `provider.ping` (which actually hits the network), but it only reads the config struct and asserts bot_token strings are non-empty. WhatsApp Web isn't checked at all.

### Fix

* Either move it to the `config` category (it isn't doing live work), or
* upgrade it to actually probe — same `getMe` / `auth.test` / `users/@me` calls the setup wizard does, factored into a shared module so the wizard and doctor share the canonical check.
* Add WhatsApp coverage: for Web, "session db file exists + non-empty"; for Cloud API, an HTTP probe to the phone-number endpoint.

## 6 · Wizard placeholder text leftovers

* Lines 3719-3720 in `setup_channels` reference the legacy `developers.facebook.com` flow with hardcoded instructions; those are still correct but read as static help, not interactive validation.
* The WhatsApp Web branch prints "1. Build with --features whatsapp-web" but never re-enters the binary build path. If the running binary doesn't have the feature, configuring WA Web is dead-on-arrival until the user rebuilds. Worth an upfront check.

## Recommended action plan

| Order | Effort | What                                                                                             | Why                                                              |
| ----- | ------ | ------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------- |
| 1     | M      | Add inline QR rendering in the WhatsApp Web client (qrcode crate; print to stderr).              | Unblocks user's reported bug; tiny dep, big UX win.              |
| 2     | M      | Add a setup-time "would you like to pair now?" path that runs the WA client and renders the QR. | Closes the loop so users don't have to start the daemon to pair. |
| 3     | S      | Validate zero-auth MCPs same as authed ones.                                                    | Catches missing `npx`/`uvx`/binary at onboarding time.           |
| 4     | S      | Move `channels.auth` to the `config` category, OR upgrade to real probes (preferred).            | Stops misleading users about what doctor verified.               |
| 5     | XS     | Runtime warning when WhatsApp Web is selected without the `whatsapp-web` feature compiled in.    | Avoids silent dead-end after re-running the wizard.              |
| 6     | XS     | Approval-preset round-trip self-check (load the file you just wrote).                            | Schema drift insurance; ~10 lines.                               |

## Files touched in this audit (read-only)

* `src/onboard/wizard.rs`
* `src/onboard/section/{channels, mcp, persona, skills, provider, approvals}.rs`
* `src/channels/whatsapp_web.rs`
* `src/channels/{telegram, discord, slack}.rs` (referenced indirectly)
* `src/mcp/{setup, oauth, curated}.rs`
* `src/doctor/checks/{channels, provider}.rs`
* `src/approval/policy_writer.rs`

No code changes in this commit — pure report.
