# Plan 280: Production-readiness handoff — read this first in a fresh session

> **This file is the context-transfer mechanism.** It exists so a session with zero
> history can pick up the production-readiness effort without re-reading the audit,
> and without inheriting a long conversation's stale context.

## Status

- **Priority**: P0 (index; not itself executable work)
- **Category**: process / index
- **Planned at**: commit `0dd4c03` (v0.27.1-alpha), 2026-09-04
- **Companion repo**: claw-ui at `d2cb5fc` (v0.3.24)

## What happened

A full production-readiness audit ran on 2026-09-04: nine parallel read-only lenses over
~255k lines of Rust and ~20k lines of TypeScript, scored against a weighted rubric
(function 30 / correctness 20 / tests 20 / security 15 / docs 10 / operability 5).

**Result: RantaiClaw 64%, claw-ui 77%, 8 verified blockers.** Every blocker and top
finding was re-opened at its source line before being recorded.

## The three places context lives (use them in this order)

1. **This file** — the map. Cheapest entry point.
2. **Per-item plan files** — `plans/281`…`plans/287` for Wave 0. Each is self-contained:
   an executor opens exactly one, does the work, opens the PR, and never needs the audit.
3. **The live dashboard** — the full report with scores, the 16-channel matrix, the MCP
   breakdown and the 38-item ledger:
   <https://claude.ai/code/artifact/a3072789-b861-4400-a1c0-4b8f44e34921>
   HTML source kept outside both repos at `~/rantaiclaw-audit/` with its own update README.
   Read it only when you need the ledger or a score — it is ~100 KB.

There is also a project memory entry (`production-readiness-audit-2026-09-04`) that loads
automatically and carries the conclusions in a few hundred words.

## Working rule for fresh sessions

**One item per session.** Executing all 38 ledger items in one conversation fills the
context with diffs, test output and CI noise that compete with the actual task. The
established loop in this repo already fits:

```
/crew plans/28N-<slug>.md      # parallel recon → one writer in a worktree → verify → PR
```

When the PR merges, return to any session and say: `update audit W0-3 merged #NNN`.
That moves the ledger row and recomputes the score from evidence — one line, cheap.

## Owner decisions already made (do not re-litigate)

- **Supported channel tier = Telegram, Discord, Slack, WhatsApp Cloud.** The other twelve
  are worked on opportunistically; whatever does not clear the promotion checklist ships
  labelled **"under development"**. (Fourth slot: WhatsApp Cloud chosen over Mattermost —
  either is defensible, but the tier stays at four.)
- **That label must be derived, never hand-written per surface.** Add a maturity field to
  `CHANNEL_CATALOG` (`src/channels/mod.rs:374`, today key+label only), serve the full
  catalog through `/api/v1/channels` (today returns only `configured`), and have claw-ui
  read it instead of its hand-synced second copy at `claw-ui/src/lib/channels.ts`. The
  Rust doc comment already records that those two surfaces "disagreed anyway" — that is
  how `/api/v1/channels` came to report 7 of 11 channels.
- **Target ~90%** via six waves; Waves 0–3 fix what reading found (→ ~77%), Waves 4–5 buy
  evidence by running and by narrowing claims (→ ~90%). Roughly 2.5–3 months.

## Wave 0 — the 8 blockers

| Ledger | Item | Where | Plan | Status |
|---|---|---|---|---|
| W0-1 | Protect `main` (all CI gates are advisory today) | GitHub settings + `ci-run.yml` | owner action, below | code half MERGED #698 `7f99101`, #699 `db7ec6a`, #700 `002a6c2`; **ruleset still OPEN — owner** |
| W0-2 | claw-ui: bump Next 16.0.10 → 16.3.4 | claw-ui repo | `plans/281` | MERGED claw-ui #108 `c51930f`, released **v0.3.25**; pin bumped in #691 `cecc8e2` |
| W0-3 | Telegram bot token reaches logs on send errors | RantaiClaw | `plans/282` | MERGED #690 `5019a9e` |
| W0-4 | Public bind stays open when a tunnel fails to start | RantaiClaw | `plans/283` | MERGED #692 `d2fd0a7` |
| W0-5 | `ui install --dir` deletes any directory holding `.git` | RantaiClaw | `plans/284` | MERGED #693 `893f30b` |
| W0-6 | TUI panics on multibyte tool output, terminal not restored | RantaiClaw | `plans/285` | MERGED #694 `290cea1` |
| W0-7 | Email owner gate accepts forged `Authentication-Results` | RantaiClaw | `plans/286` | MERGED #695 `6aa8925` (schema 26 → 27) |
| W1-4 | MCP client: undrained stderr + replies binned by id | RantaiClaw | (prerequisite of 287) | MERGED #696 `06b51fa` |
| W0-8 | Gateway respawns every MCP server per chat request | RantaiClaw | `plans/287` | MERGED #697 `288692f` |

Executed 2026-09-04 in one session. Everything above landed on green CI; two PRs went red
first (a `wrong_self_convention` on the email gate, an `AppState` literal in
`tests/kb/api_test.rs` that a `src/`-only sweep missed) and were fixed rather than merged
past.

Deviations from the plans, both recorded in the PR bodies:

- **283 step 2** (a fatal check inside the tunnel-failure arm) is unreachable once step 1
  lands: a public bind without `allow_public_bind` is refused before the listener exists.
  The arm got an honest message instead of a branch no test can reach.
- **287 step 5** asserts single-spawn at the pool handle rather than by driving two HTTP
  requests (which needs a live provider and a full router); the wiring itself is verified by
  a `discover_mcp_tools` sweep.

Order: 281–285 are independent and parallelisable. 286 and 287 are larger; take them last.

### W0-1 is an owner action, not a plan

Branch protection needs admin rights this account does not have (`gh api repos/... --jq
.permissions.admin` → `false`). In the GitHub UI: Settings → Rules → Rulesets → new branch
ruleset targeting the default branch, **Active**, with require-a-pull-request, block
force-push, block deletion, and these required status checks:

    CI Required Gate
    Intake Checks
    Conventional Commit Title
    Workflow Sanity (tabs)
    Workflow Sanity (actionlint)
    Security Audit
    License & Supply Chain

The code half is done (2026-09-04). Three things had to change first, and the plan's
one-line version of this item would have produced a broken ruleset:

- **Never require a `paths:`-filtered workflow.** The plan named `Workflow Sanity`, which
  was filtered to `.github/**` — on any other PR it reports nothing, and a required check
  that never reports leaves the PR pending forever. #699 unfiltered it and `sec-audit`,
  and gave the sanity jobs stable `name:`s (they had none, so their check names were raw
  job ids).
- **The gate did not read two jobs it ran.** `channel-lark` was missing from its `needs:`,
  and `docs-quality` was only checked on pushes. #698 extracted the decision table to
  `scripts/ci/required_gate.sh` with a 19-case `--self-test` the job runs before using it.
- **`is_ok` treated `skipped` as passing**, so `features`/`bench-compile` would still not
  have blocked anything. #700 removed the `ci:full` gates and the push-only `e2e`, and
  replaced `is_ok` with `require_success`. Cost: the Rust-PR critical path moves from
  `build` (~8 min) to `bench-compile` (~11 min); `features` and `e2e` are shorter than
  `build` and cost nothing.

`ci:full` is now inert — it is not a managed label, and both CI docs say so.

## Verification baseline (disk-constrained machine)

This box has ~57 GB free and a bare `cargo test` writes ~27 GB. **Never run an unscoped
`cargo test`.** Use scoped commands, e.g.:

```bash
cargo fmt --all -- --check
cargo clippy -p rantaiclaw --lib -- -D clippy::correctness
cargo test --lib channels::telegram          # scope to the module you touched
```

The binary crate root recompiles the library as a second crate, so `cargo build --lib`
cannot catch a missed `src/main.rs` edit — build the binary when you touch command enums.

## STOP conditions for any executor

- The cited line no longer matches the excerpt in the plan → STOP and report drift.
- A fix would widen an exposure boundary (bind address, pairing, rate limits) → STOP.
- Scope grows beyond the plan's file list → STOP; open a follow-up instead.

## The execution prompt (paste this into a fresh session)

```
Kerjakan Gelombang 0 production-readiness, satu plan per PR, sampai tuntas.

BACA DULU: plans/280-production-readiness-handoff.md — peta, keputusan yang sudah
diambil, baseline verifikasi, STOP condition. Jangan lewati.

URUTAN:
1. claw-ui/plans/281-bump-next-to-16-3-4.md   (repo claw-ui; setelah rilis, bump
   CLAW_UI_RELEASE di src/webui.rs:24 sebagai PR terpisah di RantAIClaw)
2. plans/282-telegram-scrub-token-on-send-errors.md
3. plans/283-gateway-tunnel-failure-must-not-serve-public.md
4. plans/284-ui-install-must-not-delete-a-git-directory.md
5. plans/285-tui-char-safe-crops-and-panic-hook.md
6. plans/286-email-owner-gate-authentication-results.md   (bawa bump skema config)
7. plans/287-mcp-pool-owned-by-the-gateway.md   (paling besar; baca bagian Sequencing)

LOOP PER PLAN:
baca plan → drift check → branch → code → test lokal terskop → PR → tunggu CI hijau →
merge → update ledger → plan berikutnya.

ATURAN:
- Branch: nama sesuai isi pekerjaan, ikuti pola yang sudah ada (`git log --oneline -30`,
  `gh pr list --state merged --limit 20`). Jangan pakai nomor plan sebagai nama branch.
- Skill: pakai skill yang tepat untuk tiap eksekusi — `rust-skills` untuk kode Rust,
  `security-review` untuk plan 282/283/286, `superpowers:test-driven-development` saat
  menulis test, `superpowers:verification-before-completion` sebelum klaim selesai.
  Kalau belum ada yang cocok, jalankan `/find-skills` dulu.
- DISK — periksa sebelum tiap plan, bukan saat sudah gagal:
  `df -h .` (2026-09-04: sisa 7,5 GB dari 57 GB, `target/` sudah 8,7 GB).
  `cargo test` polos menulis ~27 GB — JANGAN PERNAH. Selalu terskop:
  `cargo test --lib <modul>`.
  Kalau sisa < 10 GB atau ada error "No space left on device", bersihkan bertahap:
    1. `cargo clean -p rantaiclaw`  → buang artefak crate ini saja, dependensi tetap
       terkompilasi; rebuild-nya menit, bukan puluhan menit. Coba ini dulu.
    2. `cargo clean`                → buang seluruh `target/` (~8,7 GB); rebuild penuh
       dan lambat. Hanya kalau langkah 1 belum cukup.
  Jangan pernah `cargo clean` saat build atau test sedang berjalan.
  Kalau menyentuh enum perintah, bangun crate biner juga — `cargo build --lib` tidak
  menangkap edit `src/main.rs` yang terlewat.
- Test tidak boleh vakum: setelah hijau, balik perbaikannya sebentar dan pastikan test
  baru GAGAL. Kalau tetap hijau, testnya yang salah, bukan kodenya.
- CI: jangan pernah merge saat merah. Baca kesimpulan tiap check, bukan hanya status
  agregatnya. Flake adalah bug yang diperbaiki, bukan dilewati.
- Setelah merge: catat hasilnya untuk ledger (W0-N, nomor PR) sebelum lanjut.

JANGAN HENTIKAN LOOP. Lanjut ke plan berikutnya sendiri. Berhenti hanya kalau drift
check gagal atau kontraknya menyimpang jauh dari plan — laporkan, jangan improvisasi.
Selebihnya tangani sendiri sebaik mungkin.
```

W0-1 (protect `main`) is not in that list because it is an owner action in the GitHub UI.
Do it before the first merge, or "merge only on green" is enforced by nothing.

---

## Wave 1 — correctness and secrets (written 2026-09-04 against `4b8f61e`)

W1-4 is **already done**: it landed as #696 alongside plan 287, because 287 required it. So
Wave 1 is six ledger items, split into nine plans so each PR stays small and independently
revertable.

| Ledger | Item | Where | Plan |
|---|---|---|---|
| W1-1 | History trim breaks tool-call pairs → provider 400 for the rest of the session | RantaiClaw | `plans/288`  **MERGED #703 `9204082`** |
| W1-2 | GLM parser turns a bare URL line into a shell `curl` | RantaiClaw | `plans/289`  **MERGED #704 `5e26ca9`** |
| W1-3a | MCP `env` + migrated configs written in plaintext | RantaiClaw | `plans/290`  **MERGED #708** |
| W1-3b | Autosave and cron announcements skip the scrubber | RantaiClaw | `plans/291`  **MERGED #709** |
| W1-3c | `save()` bakes env overrides into `config.toml` | RantaiClaw | `plans/292`  **MERGED #705 `43e5e42`** |
| W1-4 | MCP single reader + drained stderr | RantaiClaw | **DONE #696** |
| W1-5a | Rate-limit keyed on an unauthenticated bearer; leftmost XFF trusted | RantaiClaw | `plans/293`  **MERGED #706** |
| W1-5b | Config API re-resolves the config path; sync chat prompts a TTY | RantaiClaw | `plans/294`  **MERGED #710** |
| W1-5c | cosign verification fails open | RantaiClaw | `plans/295`  **MERGED #711** |
| W1-6 | Headless `setup` exits zero after failing, and answers every prompt "yes" | RantaiClaw | `plans/296`  **MERGED #707** |
| W1-7 | Console double-sends history; standalone binds `0.0.0.0`; health probe gated | claw-ui | `claw-ui/plans/297`  **MERGED claw-ui #109 `53d1356`** |

**Order**: 288, 289, 292, 293, 296 are independent and small — do them first, in any order.
290 carries a schema bump. 294 and 295 touch paths Wave 0 just changed, so re-read the drift
check. 291 depends on nothing but overlaps 290 conceptually (both are secret handling) — land
290 first so the two do not conflict in `schema.rs`. 297 is the only claw-ui plan and can run
in parallel with all of them.

### The Wave 1 execution prompt

```
Kerjakan Gelombang 1 production-readiness, satu plan per PR, sampai tuntas.

BACA DULU: plans/280-production-readiness-handoff.md — peta, keputusan, baseline
verifikasi, STOP condition. Gelombang 0 sudah selesai; W1-4 sudah mendarat di #696.

URUTAN (kecil dan independen dulu):
0. PEMANASAN, docs-only, tanpa plan file. Perbaiki docs/contributing/release-process.md
   baris 83-85. Kalimatnya sekarang: "`schema_drift` passing without a snapshot update is
   the machine-checkable statement that the config schema did not move, and therefore that
   the release carries no migration and rolls back cleanly." Itu KELIRU: gate membandingkan
   kode dengan snapshot yang SUDAH ter-commit, jadi ia tetap hijau di tree rilis yang
   snapshot-nya diperbarui di PR sebelumnya. Bukti: v0.28.0-alpha hijau di gate itu, tapi
   CURRENT_VERSION bergerak 26 (v0.27.1-alpha) menjadi 27 (v0.28.0-alpha) — rilis itu
   membawa migrasi dan TIDAK rollback bersih. Ganti dengan pemeriksaan rilis-ke-rilis:
   `git diff <tag-sebelumnya>..<commit-rilis> -- src/config/migrations.rs tests/snapshots/`,
   atau bandingkan CURRENT_VERSION di kedua tag. Verifikasi kalimat baru itu benar dengan
   menjalankan perintahnya pada v0.27.1-alpha..v0.28.0-alpha sebelum commit.
1. plans/288-pairing-safe-history-trim.md
2. plans/289-glm-parser-plain-url-arm.md
3. plans/292-save-must-not-persist-env-overrides.md
4. plans/293-rate-limit-key-after-auth.md
5. plans/296-headless-setup-honesty.md
6. plans/290-encrypt-secrets-at-rest.md          (bawa bump skema config)
7. plans/291-scrub-secrets-before-they-leave.md  (setelah 290)
8. plans/294-gateway-config-path-and-sync-chat-approval.md
9. plans/295-cosign-verification-mandatory.md
10. claw-ui/plans/297-chat-history-hostname-health.md   (repo claw-ui; bisa paralel)

LOOP PER PLAN:
baca plan → drift check → branch → code → test lokal terskop → PR → tunggu CI hijau →
merge → catat hasil untuk ledger → plan berikutnya.

ATURAN:
- Branch: nama sesuai isi pekerjaan, ikuti pola yang sudah ada (`git log --oneline -30`,
  `gh pr list --state merged --limit 20`). Jangan pakai nomor plan sebagai nama branch.
- Skill: pakai skill yang tepat tiap eksekusi — `rust-skills` untuk kode Rust,
  `security-review` untuk 289/290/291/293/295, `superpowers:test-driven-development`
  saat menulis test, `superpowers:verification-before-completion` sebelum klaim selesai.
  Kalau belum ada yang cocok, jalankan `/find-skills` dulu.
- DISK — periksa sebelum tiap plan: `df -h .`. Sisa < 10 GB jalankan
  `cargo clean -p rantaiclaw` dulu; kalau masih kurang baru `cargo clean` penuh.
  JANGAN PERNAH `cargo test` polos (~27 GB). Selalu `cargo test --lib <modul>`.
  Kalau menyentuh enum perintah, bangun crate biner juga.
- Test tidak boleh vakum: setelah hijau, balik perbaikannya sebentar dan pastikan test
  baru GAGAL. Kalau tetap hijau, testnya yang salah.
- BEBERAPA PLAN MENGUBAH TEST YANG ADA. Plan 293 mengubah
  `api_rate_limit_key_prefers_the_bearer_token_over_the_peer_ip` — itu memang disengaja,
  bukan regresi; nyatakan di PR body. Kalau sebuah test lama gagal, baca dulu apakah ia
  mengunci perilaku yang justru sedang diperbaiki.
- CI: jangan pernah merge saat merah. Baca kesimpulan tiap check. Flake diperbaiki,
  bukan dilewati.

JANGAN HENTIKAN LOOP. Lanjut ke plan berikutnya sendiri. Berhenti hanya kalau drift check
gagal atau kontraknya menyimpang jauh dari plan — laporkan, jangan improvisasi.
```


---

## Follow-up batch (F1/F2) — written 2026-09-05 against `d5a1bba`

Three small plans, all found *during* execution rather than during the audit: two deferred
from Wave 1 as design calls, one reported from a live agent run. F1-3 is closed with a
reason and needs no work — the env-free config loaders in `proxy_config.rs` and
`telegram.rs` are **not** redundant; removing them would trade the env-persistence bug for a
lost-update bug. That was a correction to plan 292, and it stands.

| Ledger | Item | Plan |
|---|---|---|
| F2-1 | Cron tool object params are prose, not schema — a live agent could not schedule | `plans/300` |
| F1-1 | Headless provider setup saves a config with no usable credential, exits 0 | `plans/298` |
| F1-2 | Gateway persists client-decorated messages; render instruction replays forever | `plans/299` |

### The follow-up execution prompt

```
Kerjakan batch turunan F1/F2, satu plan per PR, sampai tuntas.

BACA DULU: plans/280-production-readiness-handoff.md. Gelombang 0 dan 1 sudah selesai
dan dirilis (v0.28.0-alpha); HEAD d5a1bba, main hijau.

URUTAN:
1. plans/300-typed-schemas-for-cron-tool-objects.md
   Paling kecil dan membuka kembali kemampuan yang sedang rusak di lapangan: agen tidak
   bisa menjadwalkan apa pun karena `every_ms` tidak punya tipe yang bisa dibaca mesin.
2. plans/298-headless-provider-must-not-save-without-a-key.md
3. plans/299-gateway-must-not-persist-decorated-messages.md
   DUA REPO. Gateway dulu (toleran terhadap bentuk lama), lalu PR kecil di claw-ui yang
   mengirim field terstruktur alih-alih mendekorasi body — ikuti langkah 3 di plan itu.
   Jangan gabungkan keduanya jadi satu PR.

LOOP PER PLAN:
baca plan → drift check → branch → code → test lokal terskop → PR → tunggu CI hijau →
merge → catat hasil untuk ledger → plan berikutnya.

ATURAN:
- Branch: nama sesuai isi pekerjaan, ikuti pola yang sudah ada.
- Skill: pakai yang tepat tiap eksekusi — `rust-skills` untuk kode Rust,
  `superpowers:test-driven-development` saat menulis test,
  `superpowers:verification-before-completion` sebelum klaim selesai. Kalau belum ada yang
  cocok, `/find-skills` dulu.
- DISK: `df -h .` sebelum tiap plan. Sisa < 10 GB jalankan `cargo clean -p rantaiclaw`;
  kalau masih kurang baru `cargo clean` penuh. JANGAN PERNAH `cargo test` polos (~27 GB).
  Selalu `cargo test --lib <modul>`.
- MUTASI TIAP PARUH, BUKAN FITURNYA. Metode ini menemukan empat lubang di Gelombang 1:
  cabang env yang belum diuji, pasangan label/indeks yang saling menutupi, dan test
  invarian yang lulus di kode belum diperbaiki karena satu cap kebetulan jatuh di batas
  aman. Kalau membalik satu paruh tidak menjatuhkan test apa pun, paruh itu belum diuji.
- Kalau tidak ada test yang bisa membedakan perilaku lama dan baru, kirim tanpa test dan
  NYATAKAN itu di PR body. Jangan mengarang test yang tidak menguji apa pun.
- Plan 300 langkah 5(d) meng-assert pada `parameters_schema()` itu sendiri — itu disengaja,
  supaya skema tidak bisa berpisah lagi dari enum `Schedule`.
- CI: jangan pernah merge saat merah. Proteksi `main` MASIH belum aktif, jadi disiplin ini
  sepenuhnya manual — baca kesimpulan tiap check sebelum merge.

JANGAN HENTIKAN LOOP. Berhenti hanya kalau drift check gagal atau kontraknya menyimpang
jauh dari plan — laporkan, jangan improvisasi.
```

## F1/F2 follow-ups (2026-09-05)

| Plan | Item | PR |
|---|---|---|
| `plans/300` | Typed schemas for the cron tools' object parameters | **MERGED #712** |
| `plans/298` | Headless provider must not save without a usable key | **MERGED #713** |
| `plans/299` | Gateway must not persist decorated messages | **MERGED #714** + **claw-ui #110 `eaaa002`** |
| `plans/301` | One answer to credential reachability; doctor + gate use it | **MERGED #715** |


---

## F3-1 — `plans/301`, single plan (written 2026-09-05 against `a7fbaca`)

### Execution prompt

```
Kerjakan plans/301-credential-reachability-single-answer.md. Satu plan, satu PR.

BACA DULU: plans/280-production-readiness-handoff.md lalu plan 301 sepenuhnya.
HEAD a7fbaca, main hijau. Gelombang 0, 1, dan batch turunan F1/F2 sudah selesai.

LANGKAH 1 ADALAH GERBANG, BUKAN PEMANASAN. Sebelum menyentuh kode, telusuri
`create_provider` untuk SETIAP provider di factory dan buat tabel: provider → cara
kredensialnya benar-benar didapat → apakah `resolve_provider_credential` melihatnya.
Daftar kandidat di plan (Bedrock, Anthropic setup-token, Copilot, Codex, Qwen OAuth,
MiniMax, Gemini CLI, provider lokal) adalah TEBAKAN SAYA yang harus diverifikasi, bukan
dipercaya. Tempel tabelnya di PR body. Kalau tabel itu berbeda dari daftar tadi, tabel
yang benar.

KEMUDIAN: lengkapi has_usable_credential per mode auth, arahkan doctor
(checks/provider.rs dan checks/config.rs) ke sana, lalu HAPUS carve-out Bedrock di
main.rs. Urutan itu penting — carve-out hanya boleh hilang setelah fungsinya benar.

BATAS YANG TIDAK BOLEH DILANGGAR:
- Fungsi ini harus tetap OFFLINE dan murah. doctor memanggilnya di jalur cepat. Kalau
  sebuah mode auth hanya bisa dipastikan lewat panggilan jaringan, STOP dan laporkan.
- Jawaban salah ke dua arah sama buruknya: `true` yang keliru mengembalikan bug asli
  (install tersimpan padahal tidak bisa mengirim); `false` yang keliru memblokir install
  yang sehat. Karena itu satu test per mode auth, bukan satu test agregat.
- Bedrock adalah kasus yang TIDAK BOLEH regresi: hanya dengan env AWS terpasang,
  jawabannya harus true.

MUTASI TIAP PARUH. Hapus satu cabang auth, pastikan test-nya jatuh. Kalau menghapus
sebuah cabang tidak menjatuhkan apa pun, cabang itu belum diuji. Metode ini sudah
menemukan tujuh lubang di dua gelombang terakhir — termasuk asersi kontrak saya sendiri
yang hanya mencakup sebagian kontraknya.

VERIFIKASI SILANG YANG DIMINTA PLAN: `doctor` dan `doctor models` harus memberi verdict
yang sama untuk install yang sama. Audit menemukan keduanya berbeda; kalau setelah
perbaikan masih berbeda, itu temuan baru — laporkan.

DISK: `df -h .` dulu. Sisa < 10 GB jalankan `cargo clean -p rantaiclaw`. JANGAN PERNAH
`cargo test` polos (~27 GB). Pakai `cargo test --lib providers`, `--lib doctor`,
`--test setup_e2e`. Test yang mengubah env wajib lewat ENV_LOCK.

Branch: nama sesuai isi pekerjaan. Skill: `rust-skills`,
`superpowers:test-driven-development`, `superpowers:verification-before-completion`.
CI: jangan merge saat merah — proteksi main MASIH belum aktif, disiplin ini manual.

Berhenti hanya kalau drift check gagal atau langkah 1 menemukan sesuatu yang membuat
premis plan salah — laporkan, jangan improvisasi.
```


---

## A rule for whoever writes the next plan

Across Waves 0-1 and the F1/F2/F3 batches, the mutate-each-half method found **nine** holes.
The most frequent shape was not a coding mistake — it was **a contract assertion in one of
these plans that covered only part of its contract**:

- Plan 300 step 5(d) named `schedule`, so `delivery` and `patch` could be reverted to bare
  objects with nothing failing.
- Plan 294's env branch went untested because the assertion covered the other half.
- Plan 301 twice: the doctor wiring, and the delivery of the answer through the ping path.

**So: when a plan asks for an assertion on a contract, it must enumerate the whole contract
surface, not one instance of it.** "Assert the schema declares `every_ms` as an integer" is a
trap; "assert every object parameter this tool advertises has typed properties" is the
contract. If the plan cannot enumerate the surface, that is a sign the plan has not
understood it yet.

Two more rules earned the same way:

- **State plainly which parts of a plan are the author's guesses.** Plan 301's list of auth
  modes was guesswork, and saying so turned step 1 from a formality into the gate that
  produced the real table.
- **Mutating the feature is not mutating the fix.** "Reaches the prompt" and "is not
  persisted" are two properties and need two mutations; plan 299's first mutation missed
  because it aimed at the feature rather than at each half of the change.


---

## Wave 2 — contracts and decisions (plans written 2026-09-05 against `bf77d26`)

Eleven plans. The ledger's W2-1 was one row holding eleven separate decisions; it is split so
each lands and reverts alone.

| Order | Plan | Item | Note |
|---|---|---|---|
| 1 | `302` | W2-1a | delete uncompiled modules + the docs advertising them |
| 2 | `304` | W2-1c | Matrix decision — **must precede 308**, which would otherwise fix a channel being deleted |
| 3 | `311` | W2-6 | `/tasks` promote or disable |
| 4 | `310` | W2-5 | wire-level fixtures — land before 306 so provider changes have a net |
| 5 | `303` | W2-1b | config keys — schema bump |
| 6 | `305` | W2-1d | sandbox / audit / resource limits — **owner sign-off before the deleting half** |
| 7 | `307` | W2-4 | one observer registry |
| 8 | `306` | W2-1e | real usage + cost cap — after 307 (see 307 step 3) and after 310 |
| 9 | `312` | W2-1f | MCP supervision + reach — **owner sign-off before executing** |
| 10 | `308` | W2-2 | channel listener contract — grouped PRs |
| 11 | `309` | W2-3 | unify webhook dispatch — largest blast radius, last |

**Only one schema bump in flight at a time.** 303 certainly bumps; 305, 306 and 311 may.
Land them one at a time, never overlapping.

### The Wave 2 execution prompt

```
Kerjakan Gelombang 2 production-readiness, satu plan per PR, sampai tuntas.

BACA DULU: plans/280-production-readiness-handoff.md, termasuk bagian "A rule for whoever
writes the next plan" di akhirnya. Gelombang 0, 1, dan batch F1/F2/F3 sudah selesai dan
dirilis.

URUTAN (ada ketergantungan nyata, jangan diacak):
1.  plans/302-delete-code-that-is-not-compiled.md
2.  plans/304-matrix-decide.md              (HARUS sebelum 308)
3.  plans/311-tasks-surface-promote-or-disable.md
4.  plans/310-wire-level-test-fixtures.md   (jaring pengaman untuk 306)
5.  plans/303-retire-config-keys-that-do-nothing.md      (bump skema)
6.  plans/305-sandbox-audit-resource-limits-decide.md    (LAPOR sebelum menghapus)
7.  plans/307-one-observer-registry.md
8.  plans/306-real-token-usage-and-an-enforced-cost-cap.md
9.  plans/312-mcp-supervision-and-reach.md               (LAPOR sebelum eksekusi)
10. plans/308-channel-listener-fault-contract.md         (beberapa PR bergrup)
11. plans/309-one-dispatch-path-for-webhook-channels.md  (blast radius terbesar, terakhir)

DUA PLAN BUTUH PERSETUJUAN PEMILIK SEBELUM BAGIAN YANG TIDAK BISA DIBATALKAN:
- 305: menghubungkan audit log aman, silakan jalan. MENGHAPUS layer sandbox adalah
  keputusan produk — hasilkan keputusannya beserta bukti, tulis di PR body, lalu BERHENTI
  dan laporkan sebelum menghapus.
- 312: supervisi MCP dan jangkauan channel/cron menyentuh ekspektasi pengguna yang sudah
  tercatat di issue #282/#283. Hasilkan keputusannya, lalu BERHENTI dan laporkan.
Sembilan plan lain punya default yang direkomendasikan di plannya: kalau bukti mendukung
default itu, jalan terus dan catat alasannya di PR body. Kalau bukti MELAWAN default,
berhenti untuk plan itu dan laporkan — jangan memutuskan sendiri.

SATU BUMP SKEMA DALAM SATU WAKTU. 303 pasti membawanya; 305, 306, dan 311 mungkin. Jangan
pernah ada dua yang sedang berjalan bersamaan.

SETIAP PR MENULIS ENTRI CHANGELOG-NYA SENDIRI di bawah [Unreleased], dikelompokkan sesuai
Keep a Changelog. Rilis lalu harus menyusun ulang catatannya dari 14 PR karena tidak ada
satu pun yang melakukannya — itu kelalaian prompt saya, bukan Anda; jangan terulang.

GELOMBANG INI PANJANG, JADI DRIFT ITU NORMAL. Kalau drift check gagal: nomor baris bergeser
= lanjutkan. Premis plannya yang berubah = hentikan plan ITU dan laporkan, lalu lanjut ke
plan berikutnya. Jangan menghentikan seluruh loop karena satu plan basi.

ATURAN TETAP:
- Branch: nama sesuai isi pekerjaan, ikuti pola yang sudah ada.
- Skill: `rust-skills` untuk kode Rust, `security-review` untuk 305/306/311/312,
  `superpowers:test-driven-development` saat menulis test,
  `superpowers:verification-before-completion` sebelum klaim selesai. Belum ada yang cocok,
  jalankan `/find-skills`.
- DISK: `df -h .` sebelum tiap plan. Sisa < 10 GB jalankan `cargo clean -p rantaiclaw`;
  kalau masih kurang baru `cargo clean` penuh. JANGAN PERNAH `cargo test` polos (~27 GB).
- MUTASI TIAP PARUH PERBAIKAN, BUKAN FITURNYA. Metode ini menemukan sembilan lubang di tiga
  batch terakhir, dan yang paling sering adalah asersi kontrak di plan yang hanya mencakup
  sebagian kontraknya. Kalau sebuah plan meminta asersi kontrak, periksa apakah ia menyebut
  SELURUH permukaan kontrak — kalau tidak, perluas sendiri dan katakan di PR body.
- Kalau tidak ada test yang bisa membedakan perilaku lama dan baru, kirim tanpa test dan
  NYATAKAN di PR body. Jangan mengarang test yang tidak menguji apa pun.
- PLAN INI MENGHAPUS BANYAK KODE. Sebelum tiap penghapusan, buktikan nol pemanggil produksi
  dengan grep Anda sendiri, bukan dengan klaim di plan. Daftar di plan adalah temuan audit
  per 2026-09-04 dan bisa sudah berubah.
- CI: jangan merge saat merah. Proteksi main masih belum aktif — disiplin ini manual.

JANGAN HENTIKAN LOOP. Berhenti hanya pada dua titik persetujuan di atas, atau kalau premis
sebuah plan terbukti salah — laporkan, jangan improvisasi.
```
