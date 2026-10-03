use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Maximum time to wait for a screenshot command to complete.
const SCREENSHOT_TIMEOUT_SECS: u64 = 15;
/// Maximum base64 payload size to return (2 MB of base64 ≈ 1.5 MB image).
const MAX_BASE64_BYTES: usize = 2_097_152;

/// Tool for capturing screenshots using platform-native commands.
///
/// macOS: `screencapture`
/// Linux: tries `gnome-screenshot`, `scrot`, `import` (`ImageMagick`) in order.
pub struct ScreenshotTool {
    security: Arc<SecurityPolicy>,
    /// The AIEOS identity file the owner's prompt reads, when the operator
    /// configured one. A guest's turn may not write it (the file feeds the
    /// owner's prompt, the same rule `file_write` carries).
    identity_file: Option<std::path::PathBuf>,
}

impl ScreenshotTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self {
            security,
            identity_file: None,
        }
    }

    /// Names the AIEOS identity file, which the owner's prompt reads at every
    /// turn, as a file a guest's turn may not write. Mirrors the builder
    /// [`FileWriteTool::with_identity_file`](crate::tools::file_write::FileWriteTool::with_identity_file).
    #[must_use]
    pub fn with_identity_file(mut self, identity_file: Option<std::path::PathBuf>) -> Self {
        self.identity_file = identity_file;
        self
    }

    /// Determine the screenshot command for the current platform.
    fn screenshot_command(output_path: &str) -> Option<Vec<String>> {
        if cfg!(target_os = "macos") {
            Some(vec![
                "screencapture".into(),
                "-x".into(), // no sound
                output_path.into(),
            ])
        } else if cfg!(target_os = "linux") {
            Some(vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "if command -v gnome-screenshot >/dev/null 2>&1; then \
                         gnome-screenshot -f '{output_path}'; \
                     elif command -v scrot >/dev/null 2>&1; then \
                         scrot '{output_path}'; \
                     elif command -v import >/dev/null 2>&1; then \
                         import -window root '{output_path}'; \
                     else \
                         echo 'NO_SCREENSHOT_TOOL' >&2; exit 1; \
                     fi"
                ),
            ])
        } else {
            None
        }
    }

    /// Why a guest's turn may not write the picture to `output_path`, or `None`
    /// when it may. The same rule `file_write` applies: the owner's private files
    /// and the files that feed the owner's prompt are refused, and so is a link at
    /// the output name, which `file_write` never writes through. An owner's turn
    /// is not refused.
    ///
    /// Judged where the write lands: the canonical workspace plus the file name,
    /// and, when something is already at that name, where it resolves to, so a
    /// link to a private file is refused too. A link that resolves to nothing
    /// yet, such as one to a prompt file that does not exist, is refused as a
    /// link, and a path with no file name is refused for it names no target.
    async fn write_refusal(&self, output_path: &std::path::Path) -> Option<String> {
        if !crate::approval::guest::current_turn_is_guest() {
            return None;
        }
        let Some(file_name) = output_path.file_name() else {
            return Some("The screenshot path has no file name.".to_string());
        };
        let workspace = &self.security.workspace_dir;
        let canonical_workspace = tokio::fs::canonicalize(workspace)
            .await
            .unwrap_or_else(|_| workspace.clone());
        let mut targets = vec![canonical_workspace.join(file_name)];
        if let Ok(real) = tokio::fs::canonicalize(output_path).await {
            targets.push(real);
        }
        for target in targets {
            if let Some(denial) =
                crate::tools::guest_private_file_write_denial(&target, workspace).await
            {
                return Some(denial);
            }
            if let Some(denial) = crate::tools::guest_prompt_file_write_denial(
                &target,
                workspace,
                self.identity_file.as_deref(),
            )
            .await
            {
                return Some(denial);
            }
        }
        if tokio::fs::symlink_metadata(output_path)
            .await
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Some(format!(
                "Refusing to write through symlink: {}",
                output_path.display()
            ));
        }
        None
    }

    /// Execute the screenshot capture and return the result.
    async fn capture(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let filename = args
            .get("filename")
            .and_then(|v| v.as_str())
            .map_or_else(|| format!("screenshot_{timestamp}.png"), String::from);

        // Sanitize filename to prevent path traversal
        let safe_name = PathBuf::from(&filename).file_name().map_or_else(
            || format!("screenshot_{timestamp}.png"),
            |n| n.to_string_lossy().to_string(),
        );

        // Reject filenames with shell-breaking characters to prevent injection in sh -c
        const SHELL_UNSAFE: &[char] = &[
            '\'', '"', '`', '$', '\\', ';', '|', '&', '\n', '\0', '(', ')',
        ];
        if safe_name.contains(SHELL_UNSAFE) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Filename contains characters unsafe for shell execution".into()),
            });
        }

        let output_path = self.security.workspace_dir.join(&safe_name);
        if let Some(refusal) = self.write_refusal(&output_path).await {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(refusal),
            });
        }
        let output_str = output_path.to_string_lossy().to_string();

        let Some(mut cmd_args) = Self::screenshot_command(&output_str) else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Screenshot not supported on this platform".into()),
            });
        };

        // macOS region flags
        if cfg!(target_os = "macos") {
            if let Some(region) = args.get("region").and_then(|v| v.as_str()) {
                match region {
                    "selection" => cmd_args.insert(1, "-s".into()),
                    "window" => cmd_args.insert(1, "-w".into()),
                    _ => {} // ignore unknown regions
                }
            }
        }

        let program = cmd_args.remove(0);
        let result = tokio::time::timeout(
            Duration::from_secs(SCREENSHOT_TIMEOUT_SECS),
            tokio::process::Command::new(&program)
                .args(&cmd_args)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    if stderr.contains("NO_SCREENSHOT_TOOL") {
                        return Ok(ToolResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "No screenshot tool found. Install gnome-screenshot, scrot, or ImageMagick."
                                    .into(),
                            ),
                        });
                    }
                    return Ok(ToolResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Screenshot command failed: {stderr}")),
                    });
                }

                Self::read_and_encode(&output_path).await
            }
            Ok(Err(e)) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to execute screenshot command: {e}")),
            }),
            Err(_) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Screenshot timed out after {SCREENSHOT_TIMEOUT_SECS}s"
                )),
            }),
        }
    }

    /// Read the screenshot file and return base64-encoded result.
    async fn read_and_encode(output_path: &std::path::Path) -> anyhow::Result<ToolResult> {
        // Check file size before reading to prevent OOM on large screenshots
        const MAX_RAW_BYTES: u64 = 1_572_864; // ~1.5 MB (base64 expands ~33%)
        if let Ok(meta) = tokio::fs::metadata(output_path).await {
            if meta.len() > MAX_RAW_BYTES {
                return Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Screenshot saved to: {}\nSize: {} bytes (too large to base64-encode inline)",
                        output_path.display(),
                        meta.len(),
                    ),
                    error: None,
                });
            }
        }

        match tokio::fs::read(output_path).await {
            Ok(bytes) => {
                use base64::Engine;
                let size = bytes.len();
                let mut encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let truncated = if encoded.len() > MAX_BASE64_BYTES {
                    encoded.truncate(encoded.floor_char_boundary(MAX_BASE64_BYTES));
                    true
                } else {
                    false
                };

                let mut output_msg = format!(
                    "Screenshot saved to: {}\nSize: {size} bytes\nBase64 length: {}",
                    output_path.display(),
                    encoded.len(),
                );
                if truncated {
                    output_msg.push_str(" (truncated)");
                }
                let mime = match output_path.extension().and_then(|e| e.to_str()) {
                    Some("jpg" | "jpeg") => "image/jpeg",
                    Some("bmp") => "image/bmp",
                    Some("gif") => "image/gif",
                    Some("webp") => "image/webp",
                    _ => "image/png",
                };
                let _ = write!(output_msg, "\ndata:{mime};base64,{encoded}");

                Ok(ToolResult {
                    success: true,
                    output: output_msg,
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: format!("Screenshot saved to: {}", output_path.display()),
                error: Some(format!("Failed to read screenshot file: {e}")),
            }),
        }
    }
}

#[async_trait]
impl Tool for ScreenshotTool {
    fn name(&self) -> &str {
        "screenshot"
    }

    fn description(&self) -> &str {
        "Capture a screenshot of the current screen. Returns the file path and base64-encoded PNG data."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "filename": {
                    "type": "string",
                    "description": "Optional filename (default: screenshot_<timestamp>.png). Saved in workspace."
                },
                "region": {
                    "type": "string",
                    "description": "Optional region for macOS: 'selection' for interactive crop, 'window' for front window. Ignored on Linux."
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }
        self.capture(args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(
            SecurityPolicy::default()
                .with_autonomy(AutonomyLevel::Full)
                .with_workspace_dir(std::env::temp_dir()),
        )
    }

    /// A guest's turn may not write a picture over a prompt file, whether the
    /// file name is the prompt file's own or a link that already sits at the
    /// name the guest chose. The suite of guest cases has the owner's side.
    #[cfg(unix)]
    #[tokio::test]
    async fn screenshot_refuses_a_guest_a_name_that_leads_to_a_prompt_file() {
        let workspace = tempfile::TempDir::new().unwrap();
        std::fs::write(workspace.path().join("AGENTS.md"), "owner rules").unwrap();
        std::os::unix::fs::symlink("AGENTS.md", workspace.path().join("shot.png")).unwrap();
        let security = Arc::new(
            SecurityPolicy::default()
                .with_autonomy(AutonomyLevel::Full)
                .with_workspace_dir(workspace.path().to_path_buf()),
        );
        let tool = ScreenshotTool::new(security);

        for name in ["AGENTS.md", "shot.png"] {
            let result = crate::approval::guest::GUEST_TURN
                .scope((), async {
                    tool.execute(json!({ "filename": name })).await.unwrap()
                })
                .await;
            assert!(!result.success, "{name}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .contains("owner's prompt"),
                "{name}: {:?}",
                result.error
            );
        }
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("AGENTS.md")).unwrap(),
            "owner rules"
        );
    }

    /// A link at the output name that points at a prompt file that does not exist
    /// yet leads nowhere the checks can resolve, and the command that writes the
    /// picture would create the file. `file_write` refuses a link at its target
    /// at all, and so does a guest's screenshot. An owner's turn is not refused.
    #[cfg(unix)]
    #[tokio::test]
    async fn screenshot_refuses_a_guest_a_dangling_link_at_the_output_name() {
        let workspace = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink("HEARTBEAT.md", workspace.path().join("shot.png")).unwrap();
        let security = Arc::new(
            SecurityPolicy::default()
                .with_autonomy(AutonomyLevel::Full)
                .with_workspace_dir(workspace.path().to_path_buf()),
        );
        let tool = ScreenshotTool::new(security);

        let result = crate::approval::guest::GUEST_TURN
            .scope((), async {
                tool.execute(json!({ "filename": "shot.png" }))
                    .await
                    .unwrap()
            })
            .await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Refusing to write through symlink"),
            "{:?}",
            result.error
        );
        assert!(
            !workspace.path().join("HEARTBEAT.md").exists(),
            "the guest's turn created the prompt file"
        );

        assert_eq!(
            tool.write_refusal(&workspace.path().join("shot.png")).await,
            None,
            "control: an owner's turn is not refused"
        );
    }

    /// A path with no file name names no target to judge, so a guest's turn is
    /// refused rather than let through.
    #[tokio::test]
    async fn screenshot_refuses_a_guest_an_output_path_with_no_file_name() {
        let tool = ScreenshotTool::new(test_security());

        let guest = crate::approval::guest::GUEST_TURN
            .scope((), async {
                tool.write_refusal(std::path::Path::new("/")).await
            })
            .await;
        assert!(guest.is_some());
        assert_eq!(
            tool.write_refusal(std::path::Path::new("/")).await,
            None,
            "control: an owner's turn is not refused"
        );
    }

    /// The AIEOS identity file feeds the owner's prompt at every turn, so a
    /// guest granted `screenshot` must not overwrite it, even by a name that
    /// only resolves to the identity file. `file_write` carries the same rule
    /// via `with_identity_file`; the screenshot tool must carry the same.
    #[cfg(unix)]
    #[tokio::test]
    async fn screenshot_refuses_a_guest_the_aieos_identity_file() {
        let workspace = tempfile::TempDir::new().unwrap();
        std::fs::write(
            workspace.path().join("bot_identity.json"),
            "{\"identity\":{}}",
        )
        .unwrap();
        let identity = workspace.path().join("bot_identity.json");
        let security = Arc::new(
            SecurityPolicy::default()
                .with_autonomy(AutonomyLevel::Full)
                .with_workspace_dir(workspace.path().to_path_buf()),
        );
        let tool = ScreenshotTool::new(security).with_identity_file(Some(identity.clone()));

        // A direct link at the identity filename resolves to the identity file,
        // and the guest's turn is refused with the prompt-file wording (the
        // identity file feeds the owner's prompt).
        std::os::unix::fs::symlink("bot_identity.json", workspace.path().join("shot.png")).unwrap();
        let result = crate::approval::guest::GUEST_TURN
            .scope((), async {
                tool.execute(json!({ "filename": "shot.png" }))
                    .await
                    .unwrap()
            })
            .await;
        assert!(
            !result.success,
            "a guest's overwrite of the identity file must be refused"
        );
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("owner's prompt"),
            "{:?}",
            result.error
        );
        assert_eq!(
            std::fs::read_to_string(&identity).unwrap(),
            "{\"identity\":{}}",
            "the identity file must not be touched"
        );

        // The exact identity file name is refused too. A guest could otherwise
        // pick the name directly.
        let result2 = crate::approval::guest::GUEST_TURN
            .scope((), async {
                tool.execute(json!({ "filename": "bot_identity.json" }))
                    .await
                    .unwrap()
            })
            .await;
        assert!(!result2.success);

        assert_eq!(
            tool.write_refusal(&identity).await,
            None,
            "control: an owner's turn is not refused"
        );
    }

    #[test]
    fn screenshot_tool_name() {
        let tool = ScreenshotTool::new(test_security());
        assert_eq!(tool.name(), "screenshot");
    }

    #[test]
    fn screenshot_tool_description() {
        let tool = ScreenshotTool::new(test_security());
        assert!(!tool.description().is_empty());
        assert!(tool.description().contains("screenshot"));
    }

    #[test]
    fn screenshot_tool_schema() {
        let tool = ScreenshotTool::new(test_security());
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["filename"].is_object());
        assert!(schema["properties"]["region"].is_object());
    }

    #[test]
    fn screenshot_tool_spec() {
        let tool = ScreenshotTool::new(test_security());
        let spec = tool.spec();
        assert_eq!(spec.name, "screenshot");
        assert!(spec.parameters.is_object());
    }

    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn screenshot_command_exists() {
        let cmd = ScreenshotTool::screenshot_command("/tmp/test.png");
        assert!(cmd.is_some());
        let args = cmd.unwrap();
        assert!(!args.is_empty());
    }

    #[tokio::test]
    async fn screenshot_rejects_shell_injection_filename() {
        let tool = ScreenshotTool::new(test_security());
        let result = tool
            .execute(json!({"filename": "test'injection.png"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("unsafe for shell execution"));
    }

    #[test]
    fn screenshot_command_contains_output_path() {
        let cmd = ScreenshotTool::screenshot_command("/tmp/my_screenshot.png").unwrap();
        let joined = cmd.join(" ");
        assert!(
            joined.contains("/tmp/my_screenshot.png"),
            "Command should contain the output path"
        );
    }
}
