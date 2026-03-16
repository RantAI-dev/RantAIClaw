use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;

/// A pass-through tool that presents interactive content to the user in the
/// dashboard chat UI. The tool validates structured args and returns them as
/// output; rendering is handled entirely by the frontend.
pub struct ShowToUserTool;

#[async_trait]
impl Tool for ShowToUserTool {
    fn name(&self) -> &str {
        "show_to_user"
    }

    fn description(&self) -> &str {
        "Present interactive content to the user in the dashboard chat. \
         Use this when you need the user to see or interact with something: \
         scan a QR code, click a link, view an image, read terminal output, \
         or approve/reject an action. \
         Content types: qr_code (render QR from text data), \
         link (clickable URL card), image (inline image from URL or base64), \
         terminal (dark terminal-style output block), \
         approval (action card with approve/reject buttons), \
         browser (stream the container browser to the user via noVNC)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "type": {
                    "type": "string",
                    "enum": ["qr_code", "link", "image", "terminal", "approval", "browser"],
                    "description": "The type of content to display"
                },
                "title": {
                    "type": "string",
                    "description": "Title or heading for the content card"
                },
                "content": {
                    "type": "string",
                    "description": "Primary content: text data for qr_code, URL for link/image, output text for terminal, description for approval"
                },
                "description": {
                    "type": "string",
                    "description": "Optional secondary description or helper text"
                }
            },
            "required": ["type", "title", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let content_type = args
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if !["qr_code", "link", "image", "terminal", "approval", "browser"].contains(&content_type) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Invalid content type: '{}'. Expected one of: qr_code, link, image, terminal, approval, browser",
                    content_type
                )),
            });
        }

        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or_default();
        if title.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Missing required parameter: 'title'".into()),
            });
        }

        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or_default();
        if content.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Missing required parameter: 'content'".into()),
            });
        }

        // Content size guard: prevent bloated SSE events (512KB limit)
        if content.len() > 512 * 1024 {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Content exceeds maximum size (512KB)".into()),
            });
        }

        // Pass-through: return the args as output so the dashboard can render them
        let output = serde_json::to_string(&args).unwrap_or_default();
        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_to_user_tool_name() {
        let tool = ShowToUserTool;
        assert_eq!(tool.name(), "show_to_user");
    }

    #[test]
    fn show_to_user_tool_has_description() {
        let tool = ShowToUserTool;
        assert!(!tool.description().is_empty());
    }

    #[test]
    fn show_to_user_tool_schema_has_required_properties() {
        let tool = ShowToUserTool;
        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].get("type").is_some());
        assert!(schema["properties"].get("title").is_some());
        assert!(schema["properties"].get("content").is_some());
        assert!(schema["properties"].get("description").is_some());

        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&serde_json::Value::String("type".into())));
        assert!(required.contains(&serde_json::Value::String("title".into())));
        assert!(required.contains(&serde_json::Value::String("content".into())));
    }

    #[tokio::test]
    async fn show_to_user_execute_returns_args_as_output() {
        let tool = ShowToUserTool;
        let args = json!({
            "type": "qr_code",
            "title": "Scan QR Code",
            "content": "https://example.com/pair"
        });

        let result = tool.execute(args.clone()).await.unwrap();
        assert!(result.success);
        assert!(result.error.is_none());

        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["type"], "qr_code");
        assert_eq!(output["title"], "Scan QR Code");
        assert_eq!(output["content"], "https://example.com/pair");
    }

    #[tokio::test]
    async fn show_to_user_rejects_invalid_type() {
        let tool = ShowToUserTool;
        let result = tool
            .execute(json!({
                "type": "invalid",
                "title": "Test",
                "content": "test"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid content type"));
    }

    #[tokio::test]
    async fn show_to_user_rejects_missing_title() {
        let tool = ShowToUserTool;
        let result = tool
            .execute(json!({
                "type": "link",
                "title": "",
                "content": "https://example.com"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("title"));
    }

    #[tokio::test]
    async fn show_to_user_rejects_missing_content() {
        let tool = ShowToUserTool;
        let result = tool
            .execute(json!({
                "type": "link",
                "title": "Test Link",
                "content": ""
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("content"));
    }

    #[tokio::test]
    async fn show_to_user_accepts_all_valid_types() {
        let tool = ShowToUserTool;
        for content_type in &["qr_code", "link", "image", "terminal", "approval", "browser"] {
            let result = tool
                .execute(json!({
                    "type": content_type,
                    "title": "Test",
                    "content": "test content"
                }))
                .await
                .unwrap();
            assert!(result.success, "Failed for type: {}", content_type);
        }
    }

    #[tokio::test]
    async fn show_to_user_includes_optional_description() {
        let tool = ShowToUserTool;
        let result = tool
            .execute(json!({
                "type": "link",
                "title": "Auth Link",
                "content": "https://example.com/oauth",
                "description": "Click to authorize"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["description"], "Click to authorize");
    }
}
