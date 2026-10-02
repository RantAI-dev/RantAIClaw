use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use crate::tools::{PATH_POLICY_REMEDIATION, RATE_LIMIT_REMEDIATION};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

/// Write file contents with path sandboxing
pub struct FileWriteTool {
    security: Arc<SecurityPolicy>,
    /// The AIEOS identity file the owner's prompt reads, when the operator
    /// configured one. A guest's turn may not write it.
    identity_file: Option<std::path::PathBuf>,
}

impl FileWriteTool {
    pub fn new(security: Arc<SecurityPolicy>) -> Self {
        Self {
            security,
            identity_file: None,
        }
    }

    /// Names the AIEOS identity file, which the owner's prompt reads at every
    /// turn, as a file a guest's turn may not write.
    #[must_use]
    pub fn with_identity_file(mut self, identity_file: Option<std::path::PathBuf>) -> Self {
        self.identity_file = identity_file;
        self
    }
}

impl FileWriteTool {
    /// Why a write into `resolved_parent` must be refused, or `None` when it may
    /// go ahead. `resolved_parent` is canonical, or a canonical directory plus
    /// names that do not exist yet.
    async fn refusal_for(
        &self,
        resolved_parent: &std::path::Path,
        file_name: &std::ffi::OsStr,
    ) -> Option<String> {
        if !self.security.is_resolved_path_allowed(resolved_parent) {
            return Some(format!(
                "Resolved path escapes workspace: {}",
                resolved_parent.display()
            ));
        }

        let resolved_target = resolved_parent.join(file_name);
        if let Some(denial) = crate::tools::guest_private_file_write_denial(
            &resolved_target,
            &self.security.workspace_dir,
        )
        .await
        {
            return Some(denial);
        }
        crate::tools::guest_prompt_file_write_denial(
            &resolved_target,
            &self.security.workspace_dir,
            self.identity_file.as_deref(),
        )
        .await
    }
}

/// Split `parent` into its nearest ancestor that exists on disk and the names
/// below it that do not. A symlink counts as existing, even a dangling one, so
/// the caller resolves it rather than treating its name as a new directory.
async fn nearest_existing_ancestor(
    parent: &std::path::Path,
) -> (std::path::PathBuf, Vec<std::ffi::OsString>) {
    let mut existing = parent.to_path_buf();
    let mut missing = Vec::new();
    while tokio::fs::symlink_metadata(&existing).await.is_err() {
        let Some(name) = existing.file_name().map(std::ffi::OsString::from) else {
            break;
        };
        missing.push(name);
        if !existing.pop() {
            break;
        }
    }
    missing.reverse();
    (existing, missing)
}

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write contents to a file in the workspace"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative path to the file within the workspace"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'path' parameter"))?;

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'content' parameter"))?;

        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Rate limit exceeded: too many actions in the last hour.{RATE_LIMIT_REMEDIATION}"
                )),
            });
        }

        // Security check: validate path is within workspace
        if !self.security.is_path_allowed(path) {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Path not allowed by security policy: {path}{PATH_POLICY_REMEDIATION}"
                )),
            });
        }

        let full_path = self.security.workspace_dir.join(path);

        let Some(parent) = full_path.parent() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Invalid path: missing parent directory".into()),
            });
        };

        let Some(file_name) = full_path.file_name() else {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Invalid path: missing file name".into()),
            });
        };

        // Check where the write would land BEFORE creating anything, so a write
        // that a later check refuses leaves no directories behind (not even
        // outside the workspace, through a symlink in the path). Directories
        // that do not exist yet cannot be symlinks, so the nearest existing
        // ancestor, resolved, plus the missing names is where the file ends up.
        let (existing, missing) = nearest_existing_ancestor(parent).await;
        let resolved_existing = match tokio::fs::canonicalize(&existing).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to resolve file path: {e}")),
                });
            }
        };
        let would_be_parent = missing
            .iter()
            .fold(resolved_existing, |dir, name| dir.join(name));
        if let Some(error) = self.refusal_for(&would_be_parent, file_name).await {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        // Ensure parent directory exists
        tokio::fs::create_dir_all(parent).await?;

        // Resolve parent AFTER creation to block symlink escapes.
        let resolved_parent = match tokio::fs::canonicalize(parent).await {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to resolve file path: {e}")),
                });
            }
        };

        // Same checks again on the real result, in case the tree changed
        // between the check above and the creation.
        if let Some(error) = self.refusal_for(&resolved_parent, file_name).await {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            });
        }

        let resolved_target = resolved_parent.join(file_name);

        // If the target already exists and is a symlink, refuse to follow it
        if let Ok(meta) = tokio::fs::symlink_metadata(&resolved_target).await {
            if meta.file_type().is_symlink() {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Refusing to write through symlink: {}",
                        resolved_target.display()
                    )),
                });
            }
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Rate limit exceeded: action budget exhausted.{RATE_LIMIT_REMEDIATION}"
                )),
            });
        }

        match tokio::fs::write(&resolved_target, content).await {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!("Written {} bytes to {path}", content.len()),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("Failed to write file: {e}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn test_security(workspace: std::path::PathBuf) -> Arc<SecurityPolicy> {
        Arc::new(
            SecurityPolicy::default()
                .with_autonomy(AutonomyLevel::Supervised)
                .with_workspace_dir(workspace),
        )
    }

    fn test_security_with(
        workspace: std::path::PathBuf,
        autonomy: AutonomyLevel,
        max_actions_per_hour: u32,
    ) -> Arc<SecurityPolicy> {
        Arc::new(
            SecurityPolicy::default()
                .with_autonomy(autonomy)
                .with_workspace_dir(workspace)
                .with_max_actions_per_hour(max_actions_per_hour),
        )
    }

    /// Runs `turn` as dispatch runs a guest's: the guest marker, under the
    /// conversation-scoped view a guest gets.
    async fn as_guest<F: std::future::Future>(turn: F) -> F::Output {
        crate::approval::guest::GUEST_TURN
            .scope(
                (),
                crate::memory::MEMORY_VIEW
                    .scope(crate::memory::MemoryView::Only("chat:guest".into()), turn),
            )
            .await
    }

    /// Runs `turn` as an owner asking in a group: the same conversation-scoped
    /// view a guest gets, and no guest marker.
    async fn as_owner_in_a_group<F: std::future::Future>(turn: F) -> F::Output {
        crate::memory::MEMORY_VIEW
            .scope(
                crate::memory::MemoryView::Only("telegram:group-a".into()),
                turn,
            )
            .await
    }

    #[test]
    fn file_write_name() {
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        assert_eq!(tool.name(), "file_write");
    }

    #[test]
    fn file_write_schema_has_path_and_content() {
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["path"].is_object());
        assert!(schema["properties"]["content"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("path")));
        assert!(required.contains(&json!("content")));
    }

    #[tokio::test]
    async fn file_write_creates_file() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool
            .execute(json!({"path": "out.txt", "content": "written!"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("8 bytes"));

        let content = tokio::fs::read_to_string(dir.join("out.txt"))
            .await
            .unwrap();
        assert_eq!(content, "written!");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_creates_parent_dirs() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write_nested");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool
            .execute(json!({"path": "a/b/c/deep.txt", "content": "deep"}))
            .await
            .unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(dir.join("a/b/c/deep.txt"))
            .await
            .unwrap();
        assert_eq!(content, "deep");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_overwrites_existing() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write_overwrite");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("exist.txt"), "old")
            .await
            .unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool
            .execute(json!({"path": "exist.txt", "content": "new"}))
            .await
            .unwrap();
        assert!(result.success);

        let content = tokio::fs::read_to_string(dir.join("exist.txt"))
            .await
            .unwrap();
        assert_eq!(content, "new");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_blocks_path_traversal() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write_traversal");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool
            .execute(json!({"path": "../../etc/evil", "content": "bad"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("not allowed"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_blocks_absolute_path() {
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        let result = tool
            .execute(json!({"path": "/etc/evil", "content": "bad"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("not allowed"));
    }

    #[tokio::test]
    async fn file_write_missing_path_param() {
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        let result = tool.execute(json!({"content": "data"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn file_write_missing_content_param() {
        let tool = FileWriteTool::new(test_security(std::env::temp_dir()));
        let result = tool.execute(json!({"path": "file.txt"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn file_write_empty_content() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write_empty");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool
            .execute(json!({"path": "empty.txt", "content": ""}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("0 bytes"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn file_write_blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("rantaiclaw_test_file_write_symlink_escape");
        let workspace = root.join("workspace");
        let outside = root.join("outside");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();

        symlink(&outside, workspace.join("escape_dir")).unwrap();

        let tool = FileWriteTool::new(test_security(workspace.clone()));
        let result = tool
            .execute(json!({"path": "escape_dir/hijack.txt", "content": "bad"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("escapes workspace"));
        assert!(!outside.join("hijack.txt").exists());

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn file_write_blocks_readonly_mode() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write_readonly");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security_with(dir.clone(), AutonomyLevel::ReadOnly, 20));
        let result = tool
            .execute(json!({"path": "out.txt", "content": "should-block"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("read-only"));
        assert!(!dir.join("out.txt").exists());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_blocks_when_rate_limited() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write_rate_limited");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security_with(
            dir.clone(),
            AutonomyLevel::Supervised,
            0,
        ));
        let result = tool
            .execute(json!({"path": "out.txt", "content": "should-block"}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("Rate limit exceeded"));
        assert!(!dir.join("out.txt").exists());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── §5.1 TOCTOU / symlink file write protection tests ────

    #[cfg(unix)]
    #[tokio::test]
    async fn file_write_blocks_symlink_target_file() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join("rantaiclaw_test_file_write_symlink_target");
        let workspace = root.join("workspace");
        let outside = root.join("outside");

        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();

        // Create a file outside and symlink to it inside workspace
        tokio::fs::write(outside.join("target.txt"), "original")
            .await
            .unwrap();
        symlink(outside.join("target.txt"), workspace.join("linked.txt")).unwrap();

        let tool = FileWriteTool::new(test_security(workspace.clone()));
        let result = tool
            .execute(json!({"path": "linked.txt", "content": "overwritten"}))
            .await
            .unwrap();

        assert!(!result.success, "writing through symlink must be blocked");
        assert!(
            result.error.as_deref().unwrap_or("").contains("symlink"),
            "error should mention symlink"
        );

        // Verify original file was not modified
        let content = tokio::fs::read_to_string(outside.join("target.txt"))
            .await
            .unwrap();
        assert_eq!(content, "original", "original file must not be modified");

        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    // ── guest memory-view path rule ──────────────────────

    #[tokio::test]
    async fn file_write_denies_writing_into_memory_dir_under_guest_view() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write_guest_memory_dir");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = as_guest(async {
            tool.execute(json!({"path": "memory/x.md", "content": "leak"}))
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
                .contains("private to the owner"),
            "{:?}",
            result.error
        );
        assert!(!dir.join("memory/x.md").exists());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_allows_writing_into_memory_dir_without_guest_view() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write_no_view_memory_dir");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool
            .execute(json!({"path": "memory/x.md", "content": "fine"}))
            .await
            .unwrap();

        assert!(result.success, "control: {:?}", result.error);
        assert!(dir.join("memory/x.md").exists());

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn file_write_allows_ordinary_file_under_guest_view() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write_guest_ordinary");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = as_guest(async {
            tool.execute(json!({"path": "notes.txt", "content": "fine"}))
                .await
                .unwrap()
        })
        .await;

        assert!(result.success, "{:?}", result.error);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    // ── refused writes create nothing ────────────────────────

    /// The checks run on the nearest existing ancestor before any directory is
    /// created, so a refused write leaves no trace, even outside the workspace
    /// through a symlink.
    #[cfg(unix)]
    #[tokio::test]
    async fn file_write_refused_by_containment_creates_no_directories() {
        use std::os::unix::fs::symlink;

        let root = tempfile::TempDir::new().unwrap();
        let workspace = root.path().join("workspace");
        let outside = root.path().join("outside");
        tokio::fs::create_dir_all(&workspace).await.unwrap();
        tokio::fs::create_dir_all(&outside).await.unwrap();
        symlink(&outside, workspace.join("out")).unwrap();

        let tool = FileWriteTool::new(test_security(workspace.clone()));
        let guest = as_guest(async {
            tool.execute(json!({"path": "out/sub/f.txt", "content": "x"}))
                .await
                .unwrap()
        })
        .await;
        assert!(!guest.success);
        assert!(
            guest
                .error
                .as_deref()
                .unwrap_or("")
                .contains("escapes workspace"),
            "{:?}",
            guest.error
        );
        assert!(
            !outside.join("sub").exists(),
            "a refused write must not create directories outside the workspace"
        );

        let owner = tool
            .execute(json!({"path": "out/deeper/sub/f.txt", "content": "x"}))
            .await
            .unwrap();
        assert!(!owner.success);
        assert!(!outside.join("deeper").exists());
    }

    #[tokio::test]
    async fn file_write_refused_for_a_guest_creates_no_directories() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = FileWriteTool::new(test_security(workspace.path().to_path_buf()));

        let result = as_guest(async {
            tool.execute(json!({"path": "memory/deep/x.md", "content": "x"}))
                .await
                .unwrap()
        })
        .await;

        assert!(!result.success);
        assert!(
            !workspace.path().join("memory").exists(),
            "a refused write must not leave its directories behind"
        );
    }

    // ── guests do not write the owner's prompt files ─────────

    #[tokio::test]
    async fn file_write_denies_skill_files_under_guest_view() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = FileWriteTool::new(test_security(workspace.path().to_path_buf()));

        let result = as_guest(async {
            tool.execute(json!({"path": "skills/x/SKILL.md", "content": "planted"}))
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
                .contains("owner's prompt"),
            "{:?}",
            result.error
        );
        assert!(
            !workspace.path().join("skills/x").exists(),
            "the refused write must not create the skill directory"
        );
    }

    #[tokio::test]
    async fn file_write_denies_prompt_files_under_guest_view() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = FileWriteTool::new(test_security(workspace.path().to_path_buf()));

        for name in [
            "AGENTS.md",
            "SOUL.md",
            "TOOLS.md",
            "IDENTITY.md",
            "HEARTBEAT.md",
        ] {
            let result = as_guest(async {
                tool.execute(json!({"path": name, "content": "planted"}))
                    .await
                    .unwrap()
            })
            .await;
            assert!(!result.success, "{name} must be refused");
            assert!(!workspace.path().join(name).exists(), "{name} was written");
        }
    }

    /// The AIEOS identity file is read into the owner's prompt, so a guest may
    /// not write it, wherever in the workspace the operator put it, even when
    /// its directory does not exist yet. The owner may.
    #[tokio::test]
    async fn file_write_denies_the_aieos_identity_file_under_guest_view() {
        let workspace = tempfile::TempDir::new().unwrap();
        for relative in ["bot_identity.json", "config/ids/bot_identity.json"] {
            let identity = workspace.path().join(relative);
            let tool = FileWriteTool::new(test_security(workspace.path().to_path_buf()))
                .with_identity_file(Some(identity.clone()));

            let guest = as_guest(async {
                tool.execute(json!({"path": relative, "content": "{}"}))
                    .await
                    .unwrap()
            })
            .await;
            assert!(!guest.success, "{relative} must be refused");
            assert!(
                guest
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .contains("owner's prompt"),
                "{relative}: {:?}",
                guest.error
            );
            assert!(!identity.exists(), "{relative} was written");

            let owner = tool
                .execute(json!({"path": relative, "content": "{}"}))
                .await
                .unwrap();
            assert!(owner.success, "{relative}: {:?}", owner.error);
            assert!(identity.exists());
        }
    }

    #[tokio::test]
    async fn file_write_allows_notes_and_owner_prompt_files() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = FileWriteTool::new(test_security(workspace.path().to_path_buf()));

        let guest = as_guest(async {
            tool.execute(json!({"path": "notes/a.txt", "content": "fine"}))
                .await
                .unwrap()
        })
        .await;
        assert!(guest.success, "{:?}", guest.error);
        assert!(workspace.path().join("notes/a.txt").exists());

        // Control: without a guest view the same files are the owner's to write.
        let agents = tool
            .execute(json!({"path": "AGENTS.md", "content": "owner rules"}))
            .await
            .unwrap();
        assert!(agents.success, "{:?}", agents.error);
        let skill = tool
            .execute(json!({"path": "skills/x/SKILL.md", "content": "owner skill"}))
            .await
            .unwrap();
        assert!(skill.success, "{:?}", skill.error);
        assert!(workspace.path().join("skills/x/SKILL.md").exists());
    }

    /// `Only` narrows what a turn reads. It is not a mark of a guest, so an owner
    /// asking in a group keeps the write access an owner has in a direct chat.
    #[tokio::test]
    async fn file_write_lets_an_owner_in_a_group_write_prompt_and_private_files() {
        let workspace = tempfile::TempDir::new().unwrap();
        let tool = FileWriteTool::new(test_security(workspace.path().to_path_buf()));

        for path in ["skills/x/SKILL.md", "AGENTS.md", "memory/x.md", "USER.md"] {
            let result = as_owner_in_a_group(async {
                tool.execute(json!({"path": path, "content": "owner text"}))
                    .await
                    .unwrap()
            })
            .await;
            assert!(result.success, "{path}: {:?}", result.error);
            assert!(
                workspace.path().join(path).exists(),
                "{path} was not written"
            );
        }
    }

    #[tokio::test]
    async fn file_write_blocks_null_byte_in_path() {
        let dir = std::env::temp_dir().join("rantaiclaw_test_file_write_null");
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let tool = FileWriteTool::new(test_security(dir.clone()));
        let result = tool
            .execute(json!({"path": "file\u{0000}.txt", "content": "bad"}))
            .await
            .unwrap();
        assert!(!result.success, "paths with null bytes must be blocked");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
