# Dependency decisions: matrix-sdk, `whatsapp-web` in default, duplicate transport stacks

**Date**: 2026-08-14
**Status**: recommendation — **awaiting a maintainer decision**. No manifest was
changed by the PR that adds this document.
**Plan**: `plans/145-dependency-decisions.md`

Three dependency facts. None is a defect. All three are costs nobody has
consciously accepted, and the status quo on each is being chosen by inaction.

---

## 1. Measurements

Taken at `1a015d0` on x86_64-unknown-linux-gnu, rustc stable.

Crate counts — `cargo tree --edges normal --prefix none <features> | sort -u | wc -l`:

| Configuration | Crates | Δ vs default |
|---|---|---|
| `--no-default-features` | 385 | −229 |
| default minus `whatsapp-web` (`tui,remote-install,kb`) | 559 | **−55** |
| default minus `kb` (`tui,whatsapp-web,remote-install`) | 580 | −34 |
| **default** (`tui,whatsapp-web,remote-install,kb`) | **614** | — |
| default + `channel-matrix` | 716 | **+102** |

Binary sizes — `cargo build --profile release-fast --locked`, `ls -l target/release-fast/rantaiclaw`:

| Configuration | Size | Δ vs default |
|---|---|---|
| default | 33,984,848 B (32.4 MiB) | — |
| default minus `whatsapp-web` | 29,237,776 B (27.9 MiB) | **−4.5 MiB (−14.0%)** |
| default minus `kb` | 31,774,520 B (30.3 MiB) | −2.1 MiB (−6.5%) |

Notes on method:

- `release-fast` is what `pub-release.yml:320` builds, so these are comparable to
  what ships; the `dist` profile (`opt-level = "z"`, `lto = "fat"`, `strip`)
  produces smaller numbers than these.
- `cargo bloat` is not installed on this machine and installing it was out of
  scope for a measurement pass; the per-crate attribution below is from crate
  counts and lockfile closure, and is labelled as such.
- The box has 57 GB total. Each release configuration was built one at a time
  into the same `target/release-fast/` (~7-8 minutes each from a warm
  dependency cache), because release builds have exhausted this disk in past
  efforts. The debug tree was deleted first to make room.
- The 55 crates `whatsapp-web` adds include the protocol stack itself
  (`wa-rs`, `wa-rs-core`, `wa-rs-binary`, `wa-rs-proto`, `wa-rs-appstate`,
  `wa-rs-derive`, **`wa-rs-libsignal`**, **`wa-rs-noise`**,
  `wa-rs-tokio-transport`, `wa-rs-ureq-http`), its transport (`ureq`,
  `ureq-proto`, `tokio-websockets`), protobuf (`prost`, `prost-derive`,
  `protobuf`, `protobuf-support`), crypto (`x25519-dalek`, `ghash`, `hkdf`,
  `zeroize`) and a cache (`moka`, `dashmap`, `crossbeam-*`).

---

## 2. matrix-sdk — four options

`Cargo.toml:42` pins `matrix-sdk 0.16` with `e2e-encryption`. It contributes
**+102 crates** to the graph (614 → 716), 81 of them exclusive to it — the
`ruma` family, `vodozemac`'s `matrix-pickle`, `imbl`/`imbl-sized-chunks`,
`blake3`, `decancer`, `aquamarine`, `html5ever`'s successor `pulldown-cmark`,
`oauth2` and `eyeball`. It carries RUSTSEC suppressions in
`deny.toml` whose sole entry path it is, and its 1,168-line channel module is
compiled by **no CI job** — `ci-run.yml` omits `--all-features` because
matrix-sdk does not build (`error: queries overflow the depth limit!`,
reproduced locally at `1a015d0`).

Two suppressions in `deny.toml` exist only for this dependency:

- `RUSTSEC-2026-0247` — `bitmaps` via `imbl`/`imbl-sized-chunks`
- `RUSTSEC-2026-0173` — `proc-macro-error2` via `aquamarine`

`pub-release.yml:320` builds with no `--features`, so **no release binary
contains Matrix**, while `README.md:149` lists it as a channel behind a build
flag — true but easy to read as "flip a flag and it works", which it does not.

### Option 1 — wait for upstream

Zero code. Cost is indefinite and compounding: the suppressions age with the
ruma/imbl stack, and `matrix.rs` keeps drifting from a trait surface it is never
compiled against — it has already missed the entire `render_target` rollout
(`docs/reference/channels.md` §2a records Matrix as *(not wired)*).

Defensible **only** with a dated review trigger attached. It has been the answer
for months without one.

### Option 2 — `[patch.crates-io]` a fork raising the recursion limit

One line in a fork (`#![recursion_limit = "256"]` on the matrix-sdk crate root),
~6 lines in `Cargo.toml`, zero in `src/`. Restores `--all-features` CI **and**
Matrix compilation in one move.

The real cost is not the patch: `deny.toml:92` sets `unknown-git = "deny"` with
`allow-git = []`, so a git patch source needs a documented policy exception, and
`docs/contributing/actions-source-policy.md` is the precedent for how this repo
records such things. Re-forking is required on every matrix-sdk release.

### Option 3 — pin an older matrix-sdk

Blast radius unknown until attempted. `matrix.rs` uses
`RoomMessageEventContent`, `MessageType::{Text,Notice}` and the `Room`/sync API,
all of which moved across 0.1x. Realistically a day of API churn plus **new**
advisory exposure on an older ruma. Worth a two-hour timebox only if option 2's
exception is refused.

### Option 4 — drop the channel

Removes 102 crates from the all-features graph and roughly halves the
all-features type-check surface. Mechanically low risk; strategically high —
Matrix is the only E2EE self-hosted channel, it has a dedicated setup guide
(`docs/reference/matrix-e2ee-guide.md`), and the channels reference carries an
FAQ for it, which means users ask.

### Recommendation

**Option 2**, with the `deny.toml` exception written in the file with its
rationale, falling back to **option 1 with an expiry date** if the exception is
refused.

Option 4 should not happen by inaction — which is exactly what is happening now:
a channel that ships in no binary, compiles in no job, and drifts from the trait
surface every month is being removed slowly and without a decision.

---

## 3. `whatsapp-web` in the default feature set

`Cargo.toml:253` — `default = ["tui", "whatsapp-web", "remote-install", "kb"]`.

Every user therefore links `wa-rs`, `wa-rs-core`, `wa-rs-binary`, `wa-rs-proto`,
`wa-rs-ureq-http`, `wa-rs-tokio-transport`, `serde-big-array` and `prost` —
**55 crates** — including third-party pre-1.0 reimplementations of the Signal
protocol and the Noise handshake, plus a second blocking HTTP client (`ureq`).
The two other heavyweight platform channels (`channel-matrix`, `channel-lark`)
are opt-in.

`scripts/ci/check_binary_size.sh:18-22` records that this was decided once, in
v0.6.49, with the reasoning: *"WhatsApp Web is the second-largest messaging
platform globally; gating it behind a build flag broke the 'complete but still
light' thesis."* The same file keeps a 5 MB aspirational target against a ~31 MB
reality.

What has changed since that decision:

- the channel is now known to carry a reverse-engineered Signal-protocol stack,
  and `docs/reference/channels.md` §4.7 carries an account-suspension warning
  for it;
- plan 123 found and fixed a set of lifecycle and exposure defects in it;
- it had no test module until plan 141 added the storage round trips.

**The question for the maintainer**: does the v0.6.49 reasoning still hold at a
measured cost of **4.5 MiB — 14% of the binary** — and 55 crates?

Moving it out is a **user-visible packaging change** — anyone building from
source silently loses the channel — so it needs a release note, a check of the
packaging scripts, and a `README.md`/`channels.md` update in the same PR. This
document does not recommend either way: it is a product call about who the
default binary is for.

---

## 4. Duplicate transport stacks

The default binary links three HTTP client stacks and three WebSocket
implementations to do one job each:

| Stack | Versions in `Cargo.lock` | Entry path |
|---|---|---|
| `reqwest` | 0.12.28 **and** 0.13.3 | ours; 0.13 via `rig-core 0.37` (a default dep) |
| `ureq` | 3.2.0 | `wa-rs-ureq-http` |
| `tokio-tungstenite` | 0.23.1 **and** 0.28.0 | ours and the wa-rs closure |
| `tokio-websockets` | — | the wa-rs closure |

`deny.toml:96` already sets `multiple-versions = "warn"`, so this is known and
unmeasured.

**The cheap half**: replace `wa-rs-ureq-http` with a `reqwest`-backed transport
against the trait `wa-rs-core` already exposes. That removes `ureq` and
`ureq-proto` outright **and** brings WhatsApp Web traffic under the `[proxy]`
configuration it currently bypasses — which is a behaviour improvement, not just
a size one. Recommended.

**The `rig-core` reqwest 0.13 duplicate**: not worth forcing. It resolves when
rig-core's own cadence catches up; pinning around it now buys a version conflict
later.

---

## 5. Decisions — recorded 2026-08-15

Taken by the maintainer's delegation. Each is re-grounded in a measurement made
today, not in the section above, because two of that section's claims had gone
stale in the meantime (noted inline).

### 5.1 matrix-sdk → **option 1, with a review trigger**

Verified today at rustc 1.97, on `main`:

- `cargo check --lib --features channel-matrix` still fails with
  `queries overflow the depth limit!` — unchanged since `1a015d0`.
- **`recursion_limit` on *our* crate root does not help.** Probed directly: the
  limit must sit on `matrix_sdk`'s own crate root, which is what makes option 2
  a fork rather than a one-line manifest edit. This had been assumed; it is now
  measured.

Option 2 remains the technically best answer and is **not** taken here, for a
reason that is not technical: it requires hosting a fork of a security-critical
E2EE SDK, plus a `deny.toml` `unknown-git` exception. That is a maintenance
commitment for the project to make deliberately, not a side effect of a
dependency clean-up. Option 4 (drop the channel) is likewise a product call that
the section above rightly warns must not happen by inaction.

So: **wait — but stop waiting silently.** The two RUSTSEC suppressions that
exist *only* for matrix-sdk now carry an explicit review date. When it passes,
the choice is re-made rather than re-deferred.

**Stale claim corrected**: §2 says `README.md:149` reads as "flip a flag and it
works". It no longer does — plan 149's status matrix (#508) already marks Matrix
**unbuildable** in both `README.md` and `docs/reference/channels.md` §0, with
the reason. No documentation change was needed.

**Removed as dead weight**: `#![recursion_limit = "512"]` in `src/lib.rs`, whose
comment claimed it addressed exactly this problem. It does not, and no buildable
configuration needs it — verified across `--no-default-features`, `hardware`,
`browser-native`, `channel-lark`, default, `--tests` and `--bins`. If Matrix is
ever unblocked and our own monomorphizations need it, the compiler says so
immediately and it comes back with a comment that is true.

### 5.2 `whatsapp-web` stays in `default`

The v0.6.49 reasoning holds. `AGENTS.md` §3.6 is explicit that *local
capability* ships enabled so a fresh install is useful without hand-editing
config, and a channel is capability, not exposure. Moving it out is a
user-visible packaging regression — anyone building from source silently loses
the channel — traded for 4.5 MiB on a 32 MiB binary.

What the measurement changes is not the default, but what is said about it: the
account-suspension warning in `docs/reference/channels.md` §4.7 and the
reverse-engineered Signal stack are properties of a channel every user links,
and they are already documented there. Revisit if the binary target moves, not
because of crate count alone.

`scripts/ci/check_binary_size.sh`'s "5 MB target" was checked and **left alone**:
its own header already labels it aspirational, and the reachable thresholds
(35 MB error / 30 MB advisory) have been kept current.

### 5.3 `wa-rs-ureq-http` → reqwest: **do it, as its own plan**

Accepted in principle — it removes `ureq` and `ureq-proto` and brings WhatsApp
Web traffic under `[proxy]`, which it currently bypasses. That second half is a
behaviour fix, not a size one, and is the reason to do it.

It is **not** implemented in this change, for a reason found while scoping it:
`wa-rs-core`'s `HttpClient` has a second method, `execute_streaming`, and
`wa-rs/src/download.rs:238` calls it — the media download path. A reqwest
implementation therefore needs a blocking client with the proxy configuration
replicated, on a path that handles attacker-influenced bytes. `AGENTS.md` §7.5
asks for threat notes and a rollback strategy on exactly that kind of change,
and §12 says not to ship and hope on it.

Shape for whoever picks it up:

1. Implement `wa_rs_core::net::HttpClient` for a reqwest-backed type — `execute`
   maps `HttpRequest{url,method,headers,body}` onto the async client from
   `config::build_runtime_proxy_client("channel.whatsapp_web")`.
2. `execute_streaming` is sync and returns `Box<dyn Read + Send>`; it must keep
   working or WhatsApp Web media downloads break. A blocking reqwest client
   needs the same proxy settings — do not let the two drift.
3. Swap both `UreqHttpClient::new()` call sites in `src/channels/whatsapp_web.rs`
   and drop `wa-rs-ureq-http` from `Cargo.toml`.
4. Prove the proxy is honoured: point `[proxy]` at a local recorder and assert
   the media fetch arrives there. That assertion is the deliverable — the crate
   removal is incidental.

The `rig-core` reqwest 0.13 duplicate stays unforced, per §4.

### 5.4 Plan 149's `channel verify` harness → **not built**

Recorded here because it turns on the same reasoning. A per-PR job driving 17
third-party services contradicts `AGENTS.md` §3.7, which requires deterministic
CI and no unguarded network dependence: it would be red more often than green,
and a gate that is usually red trains people to ignore it.

What replaces it is what plan 149 already shipped: the per-channel verification
status matrix in `docs/reference/channels.md` §0, kept current by hand. A
manually-dispatched verification workflow remains available later if someone
wants one — the objection is to it being per-PR and required, not to it
existing.

---

## 6. What this document does not do

No manifest was changed by the write-up itself. Per the plan, implementation
waited on a recorded decision — a PR that implements an option and describes it
as obvious is the failure mode the plan exists to prevent. §5 is that record.

One follow-up applies **whichever** matrix option wins: add a `channel-matrix`
entry to the CI features matrix so the module is at minimum type-checked, and
make the release configuration state explicitly whether Matrix ships. Both touch
`.github/workflows/**`, which the current effort is instructed not to edit, so
they are recorded here rather than done.
