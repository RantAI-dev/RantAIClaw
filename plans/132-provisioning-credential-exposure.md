# Plan 132: Provisioning stops exposing credentials

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/onboard/provision/ src/onboard/wizard.rs`
>
>
> **Line numbers in this plan WILL have drifted** if an earlier plan merged
> first. That is expected and is not a stop condition. Relocate by symbol name
> (function, constant, struct) and continue. STOP only if the *code itself*
> no longer matches the "Current state" excerpt semantically — i.e. the logic
> changed, not its position.

## Status

- **Priority**: P1
- **Effort**: M
- **Risk**: LOW
- **Depends on**: none. Plan 143 runs first and has already written the probe-host test this plan un-ignores.
- **Category**: security
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

The provisioning wizard is the code that takes a channel credential from an
operator and puts it on disk. It is the worst-handled credential surface in the
codebase, in four independent ways, and one of them sends a live token to a third
party on every run.

None of this is hypothetical or hard to trigger: typing a bot token during
`rantaiclaw setup` echoes it to the terminal; a DNS hiccup during validation prints
it into the log; setting up Linq transmits the Partner API token to a domain the
project does not control; and the QQ probe reports "Credentials validated" for any
input at all.

**Rotation is part of this fix, not an afterthought.** A credential that has been
echoed to a terminal, written to a log, or sent to the wrong host is burned.
Removing the code that exposed it does not un-expose it.

## Current state

### 1. Thirteen credential prompts echo what is typed

`src/onboard/wizard.rs` uses `Input::new()…interact_text()`, which echoes, at:
`:3561` (Telegram bot token), `:3660` (Discord bot token), `:3759` and `:3816`
(Slack bot + app tokens), `:3938` (Matrix access token), `:4155` (WhatsApp access
token), `:4251` (Linq API token), `:4415`, `:4420`, `:4425` (IRC server / NickServ /
SASL passwords), `:4523` (DingTalk client secret), `:4592` (QQ app secret), `:4660`
(Lark app secret).

The masking API is already imported and used in the same file — `:310-311` uses
`Password::new()` for the console login password. `grep -c 'Password::new()'
src/onboard/wizard.rs` returns **2**.

The TUI provisioner path gets this right: 19 prompts carry `secret: true` and are
masked at `src/tui/widgets/setup_overlay.rs:483-487`. Only the CLI wizard is wrong.

### 2. Credentials are interpolated into probe URLs, and the URL becomes the error

`src/onboard/provision/validate/http.rs:17`:

```rust
    let resp = rb.send().await.with_context(|| format!("GET {url}"))?;
```

`src/onboard/provision/channels/telegram.rs:94`:

```rust
        let validate_url = format!("https://api.telegram.org/bot{}/getMe", bot_token.trim());
```

and `:112-117` renders `format!("Could not validate token (network error): {e}. Continuing…")`
into a `ProvisionEvent::Message`, which the TUI appends verbatim to its overlay log
(`src/tui/widgets/setup_overlay.rs:77-85`) and the headless driver prints to stdout
(`src/main.rs:2979`).

The gateway path already knows this hazard — `src/gateway/config_api.rs:604-605`
carries the comment "`e` does not contain the token."

Ten more provisioners share the pattern: `dingtalk.rs:124`, `discord.rs:94`,
`linq.rs:94`, `matrix.rs:120`, `mattermost.rs:119`, `nextcloud_talk.rs:122`,
`qq.rs:119`, `whatsapp_cloud.rs:126`, plus `probe_post` in `lark.rs:130` and
`slack.rs:114`.

### 3. The Linq probe targets a domain the project does not own

`src/onboard/provision/channels/linq.rs:95`:

```rust
            "https://api.linq.com/v1/account",
```

`src/channels/linq.rs:23` — what the runtime actually uses:

```rust
const LINQ_API_BASE: &str = "https://api.linqapp.com/api/partner/v3";
```

`api.linq.com` and `api.linqapp.com` are different domains. The probe sends
`Authorization: Bearer <partner token>` to the wrong one, and can never succeed —
so Linq setup always warns "Token may be invalid" even for a valid token.

**This class was fixed once already.** The 2026-07-31 provider-endpoint effort found
setup sending API keys to two unregistered domains and shipped the fix in
v0.16.1-alpha. Nothing was added to stop it recurring, and it recurred.

### 4. The DingTalk secret travels in a query string

`src/onboard/provision/channels/dingtalk.rs:118-122` builds
`…/oauth2/accessToken?appkey={}&appsecret={}` and calls `probe_get`. The CLI wizard
path does it correctly, POSTing a JSON body at `src/onboard/wizard.rs:4530-4537`.

### 5. The QQ probe validates nothing and says it did

`src/onboard/provision/channels/qq.rs:121` sends `format!("Bot {}", app_id.trim())` —
the App **ID**, not a token; `app_secret` is never sent. `:125` accepts
`status == 200 || status == 401` as `Severity::Success, "Credentials validated."`
An unauthenticated request to that endpoint returns 401, so any input passes.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Provision tests | `cargo test --lib onboard::provision` | all pass |
| Onboard tests | `cargo test --test setup_orchestration` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**:
- `src/onboard/wizard.rs` — the 13 credential prompts (step 1 only)
- `src/onboard/provision/validate/http.rs` — probe error context
- `src/onboard/provision/channels/linq.rs` — probe host
- `src/onboard/provision/channels/dingtalk.rs` — secret out of the query string
- `src/onboard/provision/channels/qq.rs` — the probe itself
- `src/main.rs` — **only** the raw pairing-payload print at `:3002-3006`

**Out of scope**:
- Making a failed probe block the write, the empty-allowlist-means-`*` mapping, and
  the nine wrong-in-a-different-way provisioners — plan 133 owns those, and it
  depends on this plan. Do not fix them here even where the file is open.
- The shared IO-helper duplication and the smoke tests — plan 134.
- The real-name identity strings in `wizard.rs` — plan 142, which is serialized
  after 133 for the same file-ownership reason.
- `src/tui/widgets/setup_overlay.rs` — its masking is already correct.

## Git workflow

- Branch: `fix/provisioning-credential-exposure`
- Conventional commits, e.g. `fix(onboard): stop echoing and logging channel credentials`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Mask every credential prompt in the CLI wizard

Replace `Input::new()…interact_text()` with `dialoguer::Password::new()` at the 13
sites listed in "Current state". Follow the existing usage at
`src/onboard/wizard.rs:310-311`.

`Password` has no `allow_empty`; for the optional prompts (the IRC passwords are
optional) use `.allow_empty_password(true)`.

Leave allowlist, hostname, port and other non-secret prompts on `Input`.

**Verify**: `cargo clippy --all-targets -- -D warnings` → exit 0, and
`grep -c 'Password::new()' src/onboard/wizard.rs` → 15.

### Step 2: Keep credentials out of probe error context

Change `probe_get` and `probe_post` in
`src/onboard/provision/validate/http.rs` so the `with_context` string cannot contain
the request URL. Two acceptable shapes — pick one and apply it consistently:

- take an explicit `label: &str` from the caller (`"Telegram getMe"`), or
- derive scheme + host from the URL and drop path and query.

Then audit every caller: any provisioner that builds a URL containing a credential
must stop doing so where a safer form exists (see step 3 and step 4), and where the
credential genuinely belongs in the path (Telegram's `getMe`), the label form
guarantees it cannot reach the message.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 3: Point the Linq probe at the real API host

Derive the probe URL from `crate::channels::linq::LINQ_API_BASE` rather than a
hand-typed literal. If that constant is not public, make it `pub(crate)` — that is
in scope, and it is the point: the two values must not be independently editable.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 4: POST the DingTalk secret instead of putting it in a query string

Switch `src/onboard/provision/channels/dingtalk.rs:118-122` to `probe_post` with a
JSON body, matching the shape the CLI wizard already uses at
`src/onboard/wizard.rs:4530-4537`.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 5: Make the QQ probe test the credential it collected

POST `appId` / `clientSecret` to the token endpoint and require a token in the
response body, mirroring the correct implementation at
`src/onboard/wizard.rs:4598-4615`. Delete the `|| result.status == 401` arm.

**Verify**: `cargo test --lib onboard::provision` → all pass.

### Step 6: Stop printing the raw pairing payload

`src/main.rs:3002-3006` prints the WhatsApp Web QR payload with a "for debugging"
caption. That payload is device-linking credential material and this is the headless
path, whose stdout is what CI and install scripts capture. Remove the print, or gate
it behind an explicit debug flag and say in the caption that it is credential
material. Keep the ASCII QR rendering.

**Verify**: `grep -n 'Raw payload' src/main.rs` returns nothing (or only a gated form).

### Step 7: Write the rotation guidance into the PR

The code change does not un-expose what is already exposed. In the PR body, state
plainly that operators should rotate:

- any channel credential typed into `rantaiclaw setup` on a shared, recorded or
  screen-shared terminal,
- any token whose setup run produced a network-error line,
- **any Linq Partner API token that has ever been through setup** — it was
  transmitted to a third-party domain,
- any DingTalk AppSecret provisioned through the TUI path,
- any WhatsApp Web device paired through the headless path.

## Test plan

New tests, in the provisioner test modules (each channel module already has a small
one — follow that placement):

1. `probe_error_context_excludes_the_url` — construct the error `probe_get` returns
   on a transport failure and assert the string contains neither a token-shaped
   substring nor a query string. Use an obviously fake token value; per repo policy
   never use a realistic one.
2. `linq_probe_host_matches_the_channel_base_host` — assert the provisioner's probe
   host equals `Url::parse(LINQ_API_BASE)`'s host. **Then generalise it**: a
   table-driven test over every provisioner that probes, asserting each one's host
   equals its channel module's configured base host. This is the test that closes
   the recurring class — plan 143 wires it into CI, but write it here.
3. `qq_probe_rejects_a_401` — a mocked 401 must not produce a `Severity::Success`
   event.
4. `dingtalk_probe_sends_no_query_string` — assert the built request has an empty
   query.

For the HTTP-shaped tests, check whether the repo already has a mock-server
dependency available before adding one; if not, extract the URL/body construction
into a pure function and assert on that rather than pulling in a new dependency
(this project treats dependency weight as a product goal).

**Mutation check (required).** For test 2, point the Linq probe back at
`api.linq.com` and confirm the test **fails** — it must now fail rather than skip,
since you removed the `#[ignore]`. Restore afterwards.

**Verify**: `cargo test --lib onboard::provision` → all pass, including the new tests.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib onboard::provision` passes, including the four new tests
- [ ] The mutation check was performed and test 2 failed as expected
- [ ] `grep -n 'interact_text' src/onboard/wizard.rs` shows no remaining hit bound to
      a token/secret/password variable
- [ ] `grep -rn 'api.linq.com' src/` returns nothing
- [ ] `grep -n 'status == 401' src/onboard/provision/channels/qq.rs` returns nothing
- [ ] `grep -n 'format!("GET {url}")' src/onboard/provision/validate/http.rs` returns nothing
- [ ] The PR body contains the rotation guidance from step 7
- [ ] No files outside the in-scope list are modified (`git status`)
- [ ] `plans/README.md` status row for 132 updated

## STOP conditions

Stop and report back if:

- `dialoguer::Password` is not available in the version this repo pins, or the
  wizard's prompt flow depends on `Input`'s validation hooks in a way `Password`
  cannot express. Do not hand-roll masking.
- The correct Linq probe endpoint is not derivable from `LINQ_API_BASE` — i.e. the
  runtime base has no account/health endpoint. In that case report it rather than
  inventing a path; a wrong path on the right host is still a failing probe.
- Any provisioner turns out to send a credential to a host you cannot match against
  a channel-module constant. That is a second instance of the Linq class and the
  operator should hear about it before you patch it.
- The generalised host test in step 2 fails for a provisioner other than Linq.
  Report the list — do not fix them silently in this plan.

## Maintenance notes

- **What interacts with this**: plan 133 makes a failed probe block the write. Until
  it lands, a probe fixed here still persists a bad credential — the two are
  complementary and 133 depends on this one.
- **What a reviewer should scrutinise**: that step 2's label form was applied to
  *every* caller, not just Telegram; a single missed site keeps the leak. And that
  no test fixture contains a realistic-looking credential — GitHub push protection
  has rejected this repo's fixtures before for exactly that.
- **Why the host test matters more than the Linq fix**: the fix is one line. The
  test is what stops this class returning a third time. Do not skip it because the
  one-line fix looks complete.
- **Deliberately deferred**: `wizard.rs` also parses ports with `unwrap_or`
  fallbacks that silently discard operator input, and the IRC/Lark TUI provisioners
  have missing prompts. Those are plan 133; they are in the same files but they are
  correctness, not credential exposure, and keeping this plan shippable on its own
  is worth the split.
