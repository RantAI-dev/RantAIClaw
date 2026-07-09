# Universal On-Demand Pairing Codes — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Work in the `feat/universal-pairing-codes` branch (worktree at `/home/shiro/rantai/rc-verify`). Run scoped tests only (the full `cargo test --workspace` OOM-kills); use `cargo +1.92.0` to match CI for lint.

**Goal:** Mint pairing codes on demand without restarting the daemon, and bring `/claim`/`/bind` self-onboarding to every multi-user channel + the gateway.

**Architecture:** A shared on-disk pairing-code store (`security::pairing_store`) decouples minting (CLI/chat/TUI) from validation (running daemon). A shared channel helper (`channels::pairing`) provides a per-channel adapter contract + generic `/bind`/`/claim` handler. Telegram is refactored onto it first as the reference; remaining channels fan out in parallel.

**Tech Stack:** Rust; existing deps `sha2`, `fs2`, `rand`, `serde_json`, `toml_edit`; clap; existing `PairingGuard` (`src/security/pairing.rs`), `Config::save` (`src/config/schema.rs:4051`).

---

## File Structure

- **Create** `src/security/pairing_store.rs` — on-disk multi-claim code store (mint/consume/prune, file-locked, hashed).
- **Modify** `src/security/mod.rs` — `pub mod pairing_store;`.
- **Create** `src/channels/pairing.rs` — `AllowlistField` enum, `PairingAdapter` contract, generic `try_handle_pairing`, `parse_pairing_command`, identity-extraction helpers, startup-code helper.
- **Modify** `src/channels/mod.rs` — `pub mod pairing;`; mint helper used by CLI; ensure `process_channel_message` (or per-channel loops) call the shared handler.
- **Modify** `src/channels/telegram.rs` — refactor `try_handle_bind`/`try_handle_claim` onto the shared core (reference impl); keep behaviour + tests.
- **Modify** each channel file (`discord.rs`, `slack.rs`, `mattermost.rs`, `matrix.rs`, `signal.rs`, `whatsapp.rs`, `whatsapp_web.rs`, `irc.rs`, `lark.rs`, `dingtalk.rs`, `qq.rs`, `linq.rs`, `nextcloud_talk.rs`, `imessage.rs`) — implement the adapter + hook the handler.
- **Modify** `src/main.rs` — `ChannelCommands::Pair { … }` CLI subcommand; dispatch.
- **Create** `src/tools/issue_pairing_code.rs` — owner-only chat tool; **Modify** `src/tools/mod.rs` (register) and `src/approval/guest.rs` (`OWNER_ONLY_TOOLS`).
- **Modify** `src/tui/commands/pairing.rs` (create) + `src/tui/commands/mod.rs` (register `/pair`).
- **Modify** `src/gateway/mod.rs` — `POST /pair` consults the store; `--channel gateway` mint path.
- **Modify** `src/skills/bundled/owner_permissions/SKILL.md` — note the invite-code flow.
- **Modify** `Cargo.toml` + `Cargo.lock` (version) + `CHANGELOG.md` (release).

---

## Phase 1 — Foundation (sequential; everything depends on it)

### Task 1: Pairing-code store

**Files:** Create `src/security/pairing_store.rs`; Modify `src/security/mod.rs`; Test: inline `#[cfg(test)]`.

- [ ] **Step 1 — failing tests.** Cover: mint returns an `XXXX-XXXX` code; `try_consume` accepts the exact code once and rejects a wrong/expired code; `max_uses` exhaustion; multi-claim within TTL (uses increments, still valid until `max_uses`/expiry); surface scoping (a "telegram" code is rejected for surface "whatsapp"); prune drops expired. Use an injected `now: i64` (do NOT call `SystemTime::now` in tests — pass `now`).

```rust
#[test]
fn mint_then_consume_within_window_multi_use() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    let code = mint(p, "telegram", 900, None, true, 1_000).unwrap(); // ttl 900s, unlimited
    // first claim ok
    let r1 = try_consume(p, "telegram", &code, 1_100).unwrap();
    assert!(r1.is_some() && r1.unwrap().grant_owner);
    // second claim still ok (multi-claim window)
    assert!(try_consume(p, "telegram", &code, 1_200).unwrap().is_some());
    // wrong surface rejected
    assert!(try_consume(p, "whatsapp", &code, 1_200).unwrap().is_none());
    // after expiry rejected
    assert!(try_consume(p, "telegram", &code, 2_000).unwrap().is_none());
}
```

- [ ] **Step 2 — run, expect fail** (`cargo test --lib security::pairing_store`).
- [ ] **Step 3 — implement.** Public API (take `now`/`ttl` as params so tests are deterministic; the runtime passes real unix time):

```rust
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

const STORE_FILE: &str = "pairing_codes.json";

#[derive(Serialize, Deserialize, Default)]
struct Store { codes: Vec<Entry> }

#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    code_hash: String,        // sha256 hex of the normalized plaintext
    surface: String,
    expires_at: i64,          // unix seconds
    max_uses: Option<u32>,    // None = unlimited within the window
    uses: u32,
    grant_owner: bool,
}

pub struct ConsumeOutcome { pub grant_owner: bool }

fn path(profile_root: &Path) -> std::path::PathBuf { profile_root.join(STORE_FILE) }

// 8 Crockford-base32 chars, grouped XXXX-XXXX. Uses `rand`.
fn generate_code() -> String { /* sample CHARSET, insert '-' at idx 4 */ }
fn hash(code: &str) -> String { /* sha2::Sha256 of code.trim().to_ascii_uppercase().replace('-', "") */ }
fn load(p: &Path) -> Store { /* read+serde; default on missing/parse-fail */ }
fn save(p: &Path, s: &Store) -> Result<()> { /* serialize; write 0600 (set perms on unix) */ }

/// Generate a code for `surface`, persist its hash, return plaintext.
pub fn mint(profile_root: &Path, surface: &str, ttl_secs: i64, max_uses: Option<u32>, grant_owner: bool, now: i64) -> Result<String> { /* lock; load; prune(now); push; save; return code */ }

/// Validate+consume a code for `surface`. Increments uses; prunes expired.
pub fn try_consume(profile_root: &Path, surface: &str, code: &str, now: i64) -> Result<Option<ConsumeOutcome>> { /* lock; load; find non-expired, uses<max, surface match, hash eq (constant_time_eq); inc uses; save; return */ }

pub fn prune(profile_root: &Path, now: i64) -> Result<()> { /* lock; load; retain not-expired & uses<max; save */ }
```
Use `fs2::FileExt` advisory lock on a `<profile>/pairing_codes.lock` file around load→modify→save. Reuse `crate::security::pairing::constant_time_eq` for hash comparison. Set file mode `0600` on unix (`std::os::unix::fs::PermissionsExt`).

- [ ] **Step 4 — run tests, expect pass.**
- [ ] **Step 5 — `cargo +1.92.0 clippy --lib -- -D clippy::correctness` clean; commit** `feat(pairing): on-disk multi-claim pairing-code store`.

### Task 2: Channel pairing helper + adapter contract

**Files:** Create `src/channels/pairing.rs`; Modify `src/channels/mod.rs`.

- [ ] **Step 1 — failing tests.** `parse_pairing_command("/claim ABCD-EFGH")` → `Some(Pairing{owner:true, code:"ABCD-EFGH"})`; `/bind X` → `owner:false`; non-command → `None`. A handler test using a fake adapter (identities `["123","alice"]`, field `AllowedUsers`) that, given a store with a minted code, mutates a passed `&mut ChannelsConfig` (append to `allowed_users` + `approval_owners` for claim) and returns a success reply.
- [ ] **Step 2 — run, expect fail.**
- [ ] **Step 3 — implement:**

```rust
pub enum AllowlistField { AllowedUsers, AllowedNumbers, AllowedFrom, AllowedSenders, AllowedContacts }

pub struct PairingCommand { pub owner: bool, pub code: String }
pub fn parse_pairing_command(text: &str) -> Option<PairingCommand> { /* trim; match "/claim "|"/bind " prefix; rest = code (charset/len sanity) */ }

/// Apply a successful pairing to config (append identities to the channel's
/// allowlist field; for owner-claim also append to approval_owners). Dedupes.
pub fn apply_pairing(cc: &mut crate::config::ChannelsConfig, channel: &str, field: AllowlistField, identities: &[String], make_owner: bool) { /* push unique into the matched Option<ChannelConfig>.<field> + approval_owners */ }

/// Full flow: parse → try_consume(store) → load+mutate+save Config → reply text.
/// Returns Some(reply) if the message WAS a pairing command (caller must not
/// forward it to the agent), None otherwise.
pub async fn try_handle_pairing(text: &str, surface: &str, field: AllowlistField, identities: &[String], profile_root: &std::path::Path) -> Option<String> { /* see steps */ }
```
`apply_pairing` matches `channel` → the right `Option<…Config>` on `ChannelsConfig` and the named field; reuse `Config::load_or_init().await` + `save().await` inside `try_handle_pairing` (mirror Telegram's `persist_*`). Owner-claim only when the consumed code's `grant_owner` is true AND `command.owner`.

- [ ] **Step 4 — tests pass.** **Step 5 — clippy clean; commit** `feat(pairing): shared channel pairing helper + adapter contract`.

### Task 3: CLI `channels pair`

**Files:** Modify `src/main.rs` (add `Pair` to `ChannelCommands` + dispatch), `src/channels/mod.rs` (handler).

- [ ] **Step 1 — failing test** (in `channels/mod.rs` tests): minting via the handler writes a code to a temp profile store and the printed code is non-empty.
- [ ] **Step 2 — run, expect fail.**
- [ ] **Step 3 — implement.** Add variant:
```rust
/// Issue an on-demand pairing code (no daemon restart needed).
Pair {
    #[arg(long, default_value = "telegram")] channel: String,
    #[arg(long, default_value_t = 15)] ttl: i64,            // minutes
    #[arg(long)] max_uses: Option<u32>,
    #[arg(long)] no_owner: bool,
},
```
Handler: resolve profile root (`crate::profile::ProfileManager::active()?.root` or the config dir), `pairing_store::mint(root, &channel, ttl*60, max_uses, !no_owner, now_unix())`, print:
```
🔐 Pairing code for <channel>: ABCD-EFGH   (valid 15 min, multi-use)
   Recipients DM the bot:  /bind ABCD-EFGH  (chat)  |  /claim ABCD-EFGH  (owner)
```
A running daemon picks it up automatically.
- [ ] **Step 4 — `./target/debug/rantaiclaw channels pair --help` + a real `--config-dir <tmp> channels pair` smoke prints a code.** **Step 5 — clippy; commit** `feat(pairing): rantaiclaw channels pair CLI`.

### Task 4: Refactor Telegram onto the shared core (reference impl)

**Files:** Modify `src/channels/telegram.rs`.

- [ ] **Step 1** — keep existing `/bind` `/claim` tests; add a test that a store-minted code (not the startup code) is accepted by the Telegram claim path.
- [ ] **Step 2 — run, expect the new test to fail.**
- [ ] **Step 3** — in `try_handle_claim`/`try_handle_bind` (hook at `telegram.rs:2070`), after the existing in-memory `PairingGuard` check, also call `channels::pairing::try_handle_pairing(text, "telegram", AllowlistField::AllowedUsers, &identities, profile_root)`. Telegram identities = `[from.id, from.username]` (already extracted at ~`telegram.rs:712`). Keep the startup `PairingGuard` code print, and ALSO mint a startup code into the store so on-demand and startup share one validation path. Preserve all existing replies.
- [ ] **Step 4 — `cargo test --lib channels::telegram` passes.** **Step 5 — clippy; commit** `refactor(pairing): telegram bind/claim via shared store`.

### Task 5: Owner-only chat tool `issue_pairing_code`

**Files:** Create `src/tools/issue_pairing_code.rs`; Modify `src/tools/mod.rs`, `src/approval/guest.rs`, `src/skills/bundled/owner_permissions/SKILL.md`.

- [ ] **Step 1 — failing tests** (mirror `tools/manage_permissions.rs` tests): `action`-free tool with params `channel`,`ttl_minutes`,`max_uses`,`owner`; minting returns success + a code in `output`; missing channel → friendly error.
- [ ] **Step 2 — run, expect fail.**
- [ ] **Step 3 — implement** a `Tool` named `issue_pairing_code` that calls `pairing_store::mint` and returns the code + `/bind`,`/claim` instructions. Add `"issue_pairing_code"` to `GuestGate::OWNER_ONLY_TOOLS` (guard-test it like `manage_permissions`). Register in `all_tools_with_runtime`. Add a "Issuing invite codes" section to the skill (blank line under headings — MD022).
- [ ] **Step 4 — `cargo test --lib tools::issue_pairing_code approval::guest skills::bundled` pass.** **Step 5 — clippy; commit** `feat(pairing): owner-only issue_pairing_code chat tool + skill`.

### Task 6: TUI `/pair`

**Files:** Create `src/tui/commands/pairing.rs`; Modify `src/tui/commands/mod.rs`.

- [ ] **Step 1 — failing tests** (mirror `tui/commands/permissions.rs`): unknown channel friendly; default shows a code. Use `block_in_place` bridge.
- [ ] **Step 2 — fail. Step 3 — implement** `/pair [channel] [--ttl N] [--no-owner]`; register `PairCommand`. **Step 4 — tests pass. Step 5 — clippy; commit** `feat(pairing): /pair TUI command`.

### Task 7: Gateway on-demand mint

**Files:** Modify `src/gateway/mod.rs` (+ `api_v1.rs` if `/pair` lives there).

- [ ] **Step 1 — failing test**: a store-minted "gateway" code is accepted by the gateway pair path.
- [ ] **Step 2 — fail. Step 3 — implement**: in the `POST /pair` handler, after the in-memory `PairingGuard::try_pair` miss, try `pairing_store::try_consume(root, "gateway", code, now)`; on hit, issue a bearer token (reuse the guard's token-generation/persist path). `channels pair --channel gateway` already mints via Task 3.
- [ ] **Step 4 — `cargo test --lib gateway` pass. Step 5 — clippy; commit** `feat(pairing): gateway on-demand pairing code via store`.

---

## Phase 2 — Channel fan-out (parallel; each independent once Phase 1 lands)

**Reference:** Task 4 (Telegram). For each channel below, in that channel's file:
1. Find where inbound text messages are handled before agent dispatch (the per-channel analog of `telegram.rs:2070`).
2. Extract the sender identity form(s) from the inbound payload (column "identity").
3. Call `channels::pairing::try_handle_pairing(text, "<surface>", AllowlistField::<field>, &identities, profile_root)`; if it returns `Some(reply)`, send the reply and **return without forwarding to the agent**.
4. At channel start, if the channel's allowlist is empty, print + store a startup code (reuse the shared startup-code helper from Task 2).
5. Add a unit test: identity extraction from a representative inbound payload; a minted code claim mutates the right allowlist field + `approval_owners`.

Each task = TDD (failing test → implement → test pass → clippy → commit `feat(pairing): <channel> /bind /claim`).

| Task | Channel(s) | surface | identity extraction | field |
|---|---|---|---|---|
| 8 | discord | discord | author.id, author.username | AllowedUsers |
| 9 | slack | slack | event.user (id) | AllowedUsers |
| 10 | mattermost | mattermost | sender user id/username | AllowedUsers |
| 11 | matrix | matrix | event sender `@user:server` | AllowedUsers |
| 12 | irc | irc | message nick | AllowedUsers |
| 13 | lark | lark | sender open_id/user_id | AllowedUsers |
| 14 | dingtalk | dingtalk | senderStaffId/userId | AllowedUsers |
| 15 | qq | qq | sender user_id | AllowedUsers |
| 16 | nextcloud_talk | nextcloud_talk | actorId/actorDisplayName | AllowedUsers |
| 17 | signal | signal | source phone (E.164) | AllowedFrom |
| 18 | whatsapp (cloud) | whatsapp | sender phone (E.164) | AllowedNumbers |
| 19 | whatsapp_web | whatsapp | sender phone (E.164) | AllowedNumbers |
| 20 | linq | linq | sender | AllowedSenders |
| 21 | imessage (macOS, `#[cfg]`) | imessage | handle (phone/email) | AllowedContacts |

Group for dispatch: agent A = tasks 8–12, agent B = 13–16, agent C = 17–19 (phone), agent D = 20–21. Each agent reads Task 4 as the pattern and the channel's existing inbound handler.

---

## Phase 3 — Integration + release (sequential)

### Task 22: Integration + lint
- [ ] Build lib + bin: `cargo build` then `cargo build --bin rantaiclaw` — clean.
- [ ] Scoped tests: `cargo test --lib security::pairing_store channels::pairing channels::telegram tools::issue_pairing_code approval::guest` — all pass.
- [ ] Lint at CI toolchain: `cargo +1.92.0 fmt --all -- --check`; `cargo +1.92.0 clippy --locked --all-targets -- -D clippy::correctness`; `BASE_SHA=$(git merge-base origin/main HEAD) bash scripts/ci/rust_strict_delta_gate.sh` — all clean.
- [ ] Commit any fixups.

### Task 23: Release 0.6.82-alpha
- [ ] Bump `Cargo.toml` + `Cargo.lock` (`rantaiclaw` version) → `0.6.82-alpha`.
- [ ] CHANGELOG entry (Added: universal `/bind` `/claim` + on-demand `channels pair` CLI/chat/TUI/gateway; no schema change).
- [ ] Commit; push branch; open PR to main.
- [ ] Wait for Build Smoke + CI Required Gate green (standard PR mode).
- [ ] `gh workflow run pub-release.yml -f release_ref=<branch> -f publish_release=false` → verify all 6 targets build (no schema/size surprise).
- [ ] Squash-merge to main; detach at origin/main; `bash scripts/release/cut_release_tag.sh v0.6.82-alpha --push`.
- [ ] Monitor `pub-release.yml` → published; confirm `gh release view v0.6.82-alpha`.

---

## Self-Review notes
- **Spec coverage:** store (T1), shared helper (T2), CLI (T3), Telegram refactor (T4), chat tool+skill (T5), TUI (T6), gateway (T7), all channels (T8–21), no-schema-change (stated), release (T22–23). All spec sections mapped.
- **No config schema change** → no migration/schema_drift gate (confirmed: allowlist fields pre-exist).
- **Type consistency:** `AllowlistField`, `try_handle_pairing(text, surface, field, identities, profile_root)`, `pairing_store::{mint,try_consume,prune}` used identically across tasks.
- **Determinism:** store functions take `now: i64` so tests don't call the clock.
