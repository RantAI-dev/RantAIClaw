# Plan 117: Hot reload carries the security half of the config, and fails loudly

> **Executor instructions**: Follow this plan step by step. Run every
> verification command and confirm the expected result before moving to the
> next step. If anything in the "STOP conditions" section occurs, stop and
> report — do not improvise. When done, update the status row for this plan
> in `plans/README.md`.
>
> **Drift check (run first)**:
> `git diff --stat f189422..HEAD -- src/channels/mod.rs`
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
- **Risk**: MED
- **Depends on**: plans/116 (serialized chain over `src/channels/mod.rs`)
- **Category**: security
- **Planned at**: commit `f189422`, 2026-08-12

## Why this matters

The hot-reload machinery reloads the cosmetic half of the channel config and pins
the security half at boot — and it does so silently, while logging that it applied
the update.

Three concrete consequences. An operator removing a compromised owner in the same
edit that leaves the provider unbuildable (an API key mid-rotation, say) gets the
removal **persisted to disk and never applied**, and because the stamp advanced, no
later message retries it. An operator setting `autonomous_tools = false` to
*re-enable* the approval gate gets "Applied updated channel runtime config from
disk" and no change at all, because that flag is read once at startup. And if the
config file is briefly unreadable — which the atomic temp-file-and-rename write
makes possible — the reload returns success with no log line whatsoever.

Under CLAUDE.md §3.5 and §3.6 these are silent-fallback-on-an-unsafe-state: the
settings that *don't* reload are precisely the ones that gate authority.

## Current state

### 1. The failure branch drops the security fields

`src/channels/mod.rs:712-731` — when the provider cannot be built, three fields are
carried forward and the stamp still advances:

```rust
            entry.defaults.autonomy_preset = …;
            entry.defaults.autonomy_level = …;
            entry.defaults.allowed_commands = …;
            …
            entry.last_applied_stamp = Some(stamp);
```

`ChannelRuntimeDefaults` (`:153-182`) also carries `approval_owners` and
`guest_gate`. Neither is copied on this branch.

Consumers: `live_approval_owners` (`:596-598`) gates who may approve at `:2159`;
`runtime_defaults.approval_owners` (`:1784-1787`) decides `sender_is_owner`, which
selects the full toolset versus the guest ceiling (`:1915-1919`).

The comment at `:700-703` reasons that the operator's fix "is itself a config write,
which changes the stamp and re-triggers this reload" — true for the provider, false
for a change that was already dropped.

### 2. Half the config is pinned at boot

Read per message from boot-time `ctx` fields rather than the reloaded defaults:
`:1921-1922` (`message_timeout_secs`, `max_tool_iterations`), `:1711`
(`auto_save_memory`), `:1770` (`min_relevance_score`).

`:3727-3738` — `channel_approval` is decided **once** from
`channels_config.autonomous_tools`, so the approval gate cannot be re-armed without
a restart.

`:3402-3420` — per-channel `allowed_users` / `mention_only` are baked into the
constructed channel objects. (Plan 115 fixed the allowlist half via
`apply_allowed_senders`; `mention_only` is still boot-pinned.)

### 3. Two silent-fallback paths

`:649-655` — both early returns are silent:

```rust
    let Some(config_path) = runtime_config_path(ctx) else {
        return Ok(());
    };
    let Some(stamp) = config_file_stamp(&config_path).await else {
        return Ok(());
    };
```

`config_file_stamp` (`:600-607`) swallows both the metadata and `modified()` errors
with `.ok()?`.

`:562-590` — `runtime_defaults_snapshot`'s fallback hardcodes
`allowed_commands: Arc::new(Vec::new())` and **guesses** `autonomy_preset` from the
coarse level, with nothing logging that the fallback was taken. It is reachable if
the two independent derivations of the config path ever disagree: `:541-546` builds
`rantaiclaw_dir.join("config.toml")` while the startup seed at `:3239-3246` keys on
`config.config_path`.

### 4. Live runtime state lives in a process-global static

`:199-202`:

```rust
static STORE: OnceLock<Mutex<HashMap<PathBuf, RuntimeConfigState>>>
```

Read/written at `:209`, `:550`, `:658`, `:709`, `:755`, `:3239` and by seven test
sites. Entries are inserted and never removed; a gateway and a channel runtime in
one process share and clobber each other's entries; tests that touch it are
order-dependent.

## Commands you will need

| Purpose | Command | Expected on success |
|---|---|---|
| Format | `cargo fmt --all -- --check` | exit 0 |
| Lint | `cargo clippy --all-targets -- -D warnings` | exit 0 |
| Channel tests | `cargo test --lib channels::` | all pass |

**Do not run a bare `cargo test`** — ~27 GB on this disk-constrained box.

## Scope

**In scope**: `src/channels/mod.rs` only.

**Out of scope**:
- The conversation-history key — plan 118. Dead code — 119. Factory — 120.
  Decomposition — 121.
- Moving the reload off the intake loop (T3-07). Its placement is load-bearing for
  owner changes applying before the next reply is authorized; changing it needs its
  own plan and its own argument.
- `src/config/schema.rs` — if a field needs to move, report it rather than editing.

## Git workflow

- Branch: `fix/hot-reload-carries-the-security-half`
- Conventional commits, e.g. `fix(channels): apply owner and guest-gate changes even when the provider fails`
- Do NOT push or open a PR unless the operator instructed it.

## Steps

### Step 1: Invert the carry-forward on the failure branch

Rather than listing which fields survive a provider-build failure, list the ones
that do **not**: `default_provider`, `model`, `api_key`, `api_url`, `reliability`.
Everything else on `ChannelRuntimeDefaults` applies.

This is the important part of the change: with the current shape, every field added
to the struct in future is silently frozen by default. Inverted, it is applied by
default, and freezing one is a deliberate act.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 2: Move the boot-pinned knobs onto the reloaded defaults

Add `message_timeout_secs`, `max_tool_iterations`, `auto_save_memory` and
`min_relevance_score` to `ChannelRuntimeDefaults`, populate them in
`load_runtime_defaults_from_config_file`, and read them from the snapshot at their
per-message use sites instead of from `ctx`.

These are behaviour knobs with no gate semantics, so this half is low-risk.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 3: Decide `autonomous_tools` explicitly, and say so

`channel_approval` is constructed once at `:3727-3738`. Two acceptable outcomes:

- **Preferred**: make it re-evaluated from the reloaded config so the gate can be
  re-armed live. This is the security-correct direction — an operator turning the
  gate back **on** must not need a restart.
- **Acceptable**: leave it boot-pinned, but on reload compare the on-disk value
  against the applied one and, when they differ, `tracing::warn!` naming the field
  and stating that a restart is required.

Do the same for `mention_only`. Whichever you choose, the config must never change
silently again.

Report which you chose and why in the PR body.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 4: Make the silent paths loud

- Log (rate-limited — this runs per message) when `config_file_stamp` returns `None`,
  naming the path and the underlying error. Have `config_file_stamp` return a
  `Result` rather than an `Option` so the reason survives.
- Log when `runtime_defaults_snapshot` takes its synthesised fallback, at `warn`,
  naming the key it looked for. That fallback hands the model a guessed autonomy
  preset the gate is not enforcing; it must never be silent.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 5: Derive the config path once

Replace the re-derivation in `runtime_config_path` with the `config_path` the
context already carries, so the reload key and the startup seed key cannot diverge.

**Verify**: `cargo test --lib channels::` → all pass.

### Step 6: Move the runtime store onto the context

Add `runtime_config: Arc<Mutex<RuntimeConfigState>>` to `ChannelRuntimeContext` —
one entry, since the context knows its own config path — and delete the static.

Preserve `runtime_defaults_snapshot`'s fallback semantics exactly; step 4 now logs
when it fires, which will tell you immediately if you changed them by accident.

Update the seven test sites to construct a context rather than poking a global. If
that turns out to be a large mechanical change, it is still in scope — the
order-dependence between those tests is a real defect.

**Verify**: `cargo test --lib channels::` → all pass.

## Test plan

New tests, modelled on the existing reload tests
(`maybe_apply_runtime_config_update_hot_reloads_owners_guest_gate_and_allowed_commands`
at `:5071` and
`maybe_apply_runtime_config_update_applies_autonomy_when_provider_build_fails` at
`:5201`).

1. `owner_removal_applies_even_when_the_provider_fails_to_build` — write a config with
   two owners and a broken provider; reload; rewrite with one owner; reload; assert
   `live_approval_owners` reflects one.
2. `guest_gate_applies_when_the_provider_fails_to_build` — same shape for
   `guest_gate`.
3. `a_new_defaults_field_is_applied_by_default` — a regression guard for step 1's
   inversion: add a field to `ChannelRuntimeDefaults` in the test build (or assert on
   the carry-forward list explicitly) so the next field added cannot be silently
   frozen.
4. `unreadable_config_logs_and_does_not_advance_the_stamp` — point the reload at a
   path that cannot be stat'd; assert it warns and that the stamp is unchanged so a
   later successful read still applies.
5. `snapshot_fallback_warns` — force the fallback and assert the warning fires.
6. `autonomous_tools_change_is_applied_or_warned` — matching whichever option step 3
   chose.
7. `reload_and_startup_agree_on_the_config_key` — assert both derive the same key.

**Mutation check (required).** For test 1, restore the original three-field
carry-forward and confirm it **fails**. For test 5, remove the warn and confirm it
**fails**. Restore both.

**Verify**: `cargo test --lib channels::` → all pass, including the seven new tests.

## Done criteria

ALL must hold:

- [ ] `cargo fmt --all -- --check` exits 0
- [ ] `cargo clippy --all-targets -- -D warnings` exits 0
- [ ] `cargo test --lib channels::` passes, including the seven new tests
- [ ] Both mutation checks performed and failed as expected
- [ ] `grep -n 'static STORE' src/channels/mod.rs` returns nothing
- [ ] The failure branch lists the fields it **excludes**, not the ones it includes
- [ ] Step 3's decision is stated in the PR body
- [ ] No files outside `src/channels/mod.rs` are modified (`git status`)
- [ ] `plans/README.md` status row for 117 updated

## STOP conditions

Stop and report back if:

- Plan 116 has not landed — this chain is serialized over one file.
- Re-evaluating `autonomous_tools` live (step 3, preferred option) turns out to
  require rebuilding the tool registry per message in a way that measurably changes
  the hot path. Take the logging option instead and say so.
- Deleting the static breaks a test in a way that reveals two runtimes genuinely
  sharing state on purpose. That would be a design fact worth surfacing, not
  something to work around.
- The two config-path derivations already disagree on this checkout — that means the
  fallback is live, not latent, and the operator should hear that immediately.
- Either mutation check still passes after you revert the fix.

## Maintenance notes

- **What interacts with this**: plan 115 added `apply_allowed_senders`, which this
  plan's step 2 sits beside; plan 121 moves this code into a `routing.rs` module.
  Land in chain order.
- **What a reviewer should scrutinise**: step 1's inversion — it is the change that
  stops this class recurring, and it is easy to implement as a longer include-list
  that looks equivalent and is not.
- **Deliberately deferred**: `get_or_create_provider` still reads boot-time
  `ctx.api_key` rather than the reloaded credentials, so a rotated key never reaches
  a sender pinned to a non-default provider via `/models`. It is the same class as
  step 2 but touches the provider cache; it is recorded as MOD-13 in the report and
  belongs in its own plan with its own credential-handling review.
