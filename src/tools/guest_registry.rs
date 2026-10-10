//! The tools a guest turn is shown.
//!
//! A guest may call only what [`GuestGate::tool_permitted`] allows, and the
//! gate refuses every other call at execution. Describing the rest to the
//! model (the tool-use protocol block, or the specs a native provider
//! receives) invites calls that are always refused. This module builds the
//! registry a guest turn runs on: the entries of the full registry the gate
//! permits.
//!
//! [`Tool`] is not `Clone`, so each entry of the narrowed list forwards to the
//! matching entry of the shared registry.

use super::traits::{Tool, ToolResult, ToolSpec};
use crate::approval::GuestGate;
use async_trait::async_trait;
use std::sync::Arc;

/// One entry of the shared registry, seen through a narrowed list.
struct RegistryEntryTool {
    registry: Arc<Vec<Box<dyn Tool>>>,
    index: usize,
}

impl RegistryEntryTool {
    fn entry(&self) -> &dyn Tool {
        self.registry[self.index].as_ref()
    }
}

#[async_trait]
impl Tool for RegistryEntryTool {
    fn name(&self) -> &str {
        self.entry().name()
    }

    fn description(&self) -> &str {
        self.entry().description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.entry().parameters_schema()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        self.entry().execute(args).await
    }

    fn spec(&self) -> ToolSpec {
        self.entry().spec()
    }

    fn is_read_only_call(&self, args: &serde_json::Value) -> bool {
        self.entry().is_read_only_call(args)
    }
}

/// The tools of `registry` that `gate` permits a guest to call, in registry
/// order. Empty when the gate permits none of them.
pub(crate) fn permitted_tools(
    registry: &Arc<Vec<Box<dyn Tool>>>,
    gate: &GuestGate,
) -> Vec<Box<dyn Tool>> {
    registry
        .iter()
        .enumerate()
        .filter(|(_, tool)| gate.tool_permitted(tool.name()))
        .map(|(index, _)| {
            Box::new(RegistryEntryTool {
                registry: Arc::clone(registry),
                index,
            }) as Box<dyn Tool>
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read-only probe. It declares every call read-only, so a forwarder
    /// that drops the declaration reads as refused.
    struct ReadDeclaringProbe;

    #[async_trait]
    impl Tool for ReadDeclaringProbe {
        fn name(&self) -> &str {
            "read_declaring_probe"
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: true,
                output: String::new(),
                error: None,
            })
        }
        fn is_read_only_call(&self, _args: &serde_json::Value) -> bool {
            true
        }
    }

    /// A guest turn runs on `permitted_tools`, not on the shared registry. If
    /// the narrowed entry did not forward the declaration, a guest's read tools
    /// would be refused under `ReadOnly` where they run today.
    #[test]
    fn registry_entry_forwards_the_read_only_declaration() {
        let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![Box::new(ReadDeclaringProbe)]);
        let gate = GuestGate::new(&["read_declaring_probe".to_string()], &[]);
        let narrowed = permitted_tools(&registry, &gate);
        assert_eq!(narrowed.len(), 1, "the gate permits the probe");
        assert!(narrowed[0].is_read_only_call(&serde_json::json!({})));
    }
}
