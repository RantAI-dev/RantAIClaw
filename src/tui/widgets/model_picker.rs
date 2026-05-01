//! Model entry — data type used by `/model` to populate the generic
//! `ListPicker`. The picker UX itself lives in `list_picker.rs`.

#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// Canonical provider name (e.g. `"openai"`).
    pub provider: String,
    /// Provider-side model id (e.g. `"gpt-5.5"`).
    pub model_id: String,
    /// Human-readable description for the row.
    pub description: String,
}

impl ModelEntry {
    /// Returns the `provider:model` string used by the runtime.
    pub fn target(&self) -> String {
        format!("{}:{}", self.provider, self.model_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_format_combines_provider_and_model() {
        let e = ModelEntry {
            provider: "openrouter".into(),
            model_id: "anthropic/claude-sonnet-4.6".into(),
            description: "via openrouter".into(),
        };
        assert_eq!(e.target(), "openrouter:anthropic/claude-sonnet-4.6");
    }
}
