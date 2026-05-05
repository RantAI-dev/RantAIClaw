# RantAIClaw Architecture Skill

RantAIClaw is trait-driven and factory-registered.

Prefer extension through:

- `src/providers/traits.rs`
- `src/channels/traits.rs`
- `src/tools/traits.rs`
- `src/memory/traits.rs`
- `src/observability/traits.rs`
- `src/runtime/traits.rs`
- `src/peripherals/traits.rs`

Avoid:

- cross-subsystem rewrites
- provider logic inside channel code
- channel logic inside provider code
- policy changes hidden in implementation code
- speculative abstractions without current callers
- broad rewrites when a trait implementation is enough

For config/schema changes:

- treat keys as public contract,
- document defaults,
- document compatibility impact,
- include migration/rollback notes.