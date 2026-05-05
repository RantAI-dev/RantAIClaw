---
description: Add or modify a RantAIClaw tool.
agent: security
---

Tools are high-risk.

Check:

- `src/tools/traits.rs`
- input schema validation
- path/shell/network boundaries
- policy checks
- structured `ToolResult` behavior
- no panics in runtime path
- tests for unsafe inputs
- docs impact

Return security-aware implementation plan first.

Do not edit unless explicitly asked after planning.