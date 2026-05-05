---
description: Security review for runtime, gateway, tools, policy, shell, filesystem, and secrets.
agent: security
---

Focus on:

- `src/security/**`
- `src/runtime/**`
- `src/gateway/**`
- `src/tools/**`
- config policy changes
- shell/file/network access
- token/secret handling
- webhook auth
- public bind behavior
- sandbox behavior
- logging of sensitive data
- CI/workflow permission changes

Do not edit files.

Return:

1. Security boundary changed?
2. Any silent permission expansion?
3. Any secret exposure risk?
4. Any unsafe default?
5. Missing tests or threat notes?
6. Verdict: safe / risky / needs changes