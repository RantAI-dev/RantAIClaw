# AGENTS.md — OpenCode Execution Protocol for RantAIClaw

`CLAUDE.md` is the canonical repository constitution. Follow it.

This file is the short operational protocol for OpenCode agents.

## Mandatory First Step

Before editing, inspect:

1. the relevant module,
2. adjacent tests,
3. factory/trait wiring,
4. docs affected by behavior changes.

Do not edit before understanding the current pattern.

## Architecture Rule

RantAIClaw is trait-driven and factory-registered.

Prefer:

- implementing existing traits,
- registering in factory modules,
- adding focused tests,
- keeping subsystem boundaries clear.

Avoid:

- cross-subsystem rewrites,
- speculative abstractions,
- hidden coupling between providers/channels/tools/runtime/security,
- provider logic inside channel code,
- channel logic inside provider code,
- tool logic mutating gateway/security policy directly.

## High-Risk Paths

Treat these as high-risk and ask before broad changes:

- `src/security/**`
- `src/runtime/**`
- `src/gateway/**`
- `src/tools/**`
- `.github/workflows/**`
- `Cargo.toml`
- `Cargo.lock`
- config schema and CLI behavior

For high-risk changes, include:

- risk note,
- failure-mode validation,
- rollback note.

## Change Discipline

- One concern per patch.
- Minimal reversible diff.
- No unrelated refactors.
- No heavy dependency unless justified.
- No silent fallback for unsafe or unsupported behavior.
- No weakening security defaults.
- No secrets, tokens, personal data, or private URLs in code/docs/tests.
- Ask before installing packages or changing dependency versions.
- Ask before changing workflow, release, CI, or deployment files.

## Validation

For code changes, prefer:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test