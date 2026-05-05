# Security Boundary Skill

High-risk surfaces:

- `src/security/**`
- `src/runtime/**`
- `src/gateway/**`
- `src/tools/**`
- `.github/workflows/**`
- config schema
- CLI commands with side effects

Rules:

- Never silently broaden permissions.
- Never log secrets or raw tokens.
- Keep shell/file/network scope narrow.
- Validate and sanitize tool inputs.
- Prefer deny-by-default behavior.
- Document intentional fallback behavior.
- Include rollback notes for risky changes.
- Include at least one failure-mode validation for high-risk changes.