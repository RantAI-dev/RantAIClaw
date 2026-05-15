//! Write-side skill management tools — Hermes-parity for the
//! "agent installs the skill for me" flow.
//!
//! Two tools registered for the LLM:
//!
//! - `skills_install` → installs a ClawHub skill by slug. Wraps
//!   `clawhub::install_one`. Requires user approval (the LLM must pass
//!   `approved: true` and the supervised-mode approval manager
//!   intercepts to ask the user).
//! - `skills_install_deps` → runs the install recipe for an already-
//!   installed-but-gated skill (brew/uv/npm/go/download). Wraps
//!   `install_deps_for_with_prefs`. Same approval gate.
//!
//! Approval is name-based via [`crate::approval::ApprovalManager`] —
//! the existing `auto_approve` / `always_ask` config keys apply. Users
//! who want zero friction can add `"skills_install"` /
//! `"skills_install_deps"` to `auto_approve`; users who want explicit
//! review keep them out (default in supervised mode).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::traits::{Tool, ToolResult};

/// `skills_install` — install a ClawHub skill from chat. Approval-gated.
pub struct SkillsInstallTool {
    profile: crate::profile::Profile,
}

impl SkillsInstallTool {
    pub fn new(profile: crate::profile::Profile) -> Self {
        Self { profile }
    }
}

#[async_trait]
impl Tool for SkillsInstallTool {
    fn name(&self) -> &str {
        "skills_install"
    }

    fn description(&self) -> &str {
        "Install a skill from ClawHub by its slug (e.g. `weather`, \
         `gog`, `humanizer`). The skill files are downloaded to the \
         active profile's skills dir and become available to the agent \
         on the next turn. Pair with `skills_search` to find the slug \
         first. Requires user approval in supervised mode — the user \
         will be prompted before the install actually runs."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "slug": {
                    "type": "string",
                    "description": "ClawHub skill slug (e.g. `weather`)."
                },
                "approved": {
                    "type": "boolean",
                    "description": "Set to true to confirm the install in supervised mode.",
                    "default": false
                }
            },
            "required": ["slug"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let slug = args
            .get("slug")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'slug' parameter"))?
            .trim();
        if slug.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("slug must be non-empty".into()),
            });
        }

        let profile = self.profile.clone();
        let slug_owned = slug.to_string();
        let result = crate::skills::clawhub::install_one(&profile, &slug_owned).await;
        match result {
            Ok(()) => Ok(ToolResult {
                success: true,
                output: format!(
                    "Installed `{slug_owned}` from ClawHub into {}. \
                     The agent will see it on the next turn — call \
                     `skills_list` if you need to confirm.",
                    profile.skills_dir().display()
                ),
                error: None,
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("clawhub install of `{slug_owned}` failed: {e:#}")),
            }),
        }
    }
}

/// `skills_install_deps` — fill missing binary deps for an
/// already-installed-but-gated skill. Approval-gated.
pub struct SkillsInstallDepsTool {
    workspace_dir: PathBuf,
    config: Arc<crate::config::Config>,
}

impl SkillsInstallDepsTool {
    pub fn new(workspace_dir: PathBuf, config: Arc<crate::config::Config>) -> Self {
        Self {
            workspace_dir,
            config,
        }
    }
}

#[async_trait]
impl Tool for SkillsInstallDepsTool {
    fn name(&self) -> &str {
        "skills_install_deps"
    }

    fn description(&self) -> &str {
        "Run the install recipe (brew/uv/npm/go/download) for an already-\
         installed skill whose declared binary dependencies aren't on \
         $PATH. Pair with `skills_list` to find which skills are gated \
         and on what bin. Requires user approval in supervised mode — \
         the runner will shell out to brew / npm / etc."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name (case-insensitive). \
                                    Must already be installed; use \
                                    `skills_install` first if not."
                },
                "approved": {
                    "type": "boolean",
                    "description": "Set to true to confirm the install in supervised mode.",
                    "default": false
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'name' parameter"))?;

        let with_status = crate::skills::load_skills_with_status(&self.workspace_dir, &self.config);
        let skill = match with_status
            .into_iter()
            .find(|(s, _)| s.name.eq_ignore_ascii_case(name))
        {
            Some((s, _)) => s,
            None => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "No installed skill named `{name}`. \
                         Use `skills_install` to install it first."
                    )),
                });
            }
        };

        if skill.install_recipes.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Skill `{}` has no install recipes declared. \
                     Install the missing binaries manually then re-run \
                     `skills_list` to verify.",
                    skill.name
                )),
            });
        }

        // Run the recipe inside spawn_blocking so the brew/npm/curl
        // subprocess doesn't tie up the tokio runtime.
        let prefs =
            crate::skills::install_deps::SelectorPrefs::from_config(&self.config.skills.install);
        let skill_for_task = skill.clone();
        let prefs_for_task = prefs;
        let outcome = tokio::task::spawn_blocking(move || {
            crate::skills::install_deps::install_deps_for_with_prefs(
                &skill_for_task,
                &prefs_for_task,
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("install-deps task panicked: {e}"))?;

        match outcome {
            Ok(o) if o.bins_installed.is_empty() && o.bins_still_missing.is_empty() => {
                Ok(ToolResult {
                    success: true,
                    output: format!(
                        "Skill `{}` already had its binaries on $PATH — nothing to install.",
                        o.skill
                    ),
                    error: None,
                })
            }
            Ok(o) if o.bins_still_missing.is_empty() => Ok(ToolResult {
                success: true,
                output: format!(
                    "Installed {} for `{}` via {}.",
                    o.bins_installed.join(", "),
                    o.skill,
                    o.recipe_used.unwrap_or_else(|| "<unknown>".into())
                ),
                error: None,
            }),
            Ok(o) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Recipe ran but `{}` is still missing: {}. \
                     The user may need to install it manually.",
                    o.skill,
                    o.bins_still_missing.join(", ")
                )),
            }),
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("install-deps recipe failed: {e:#}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write(p: &std::path::Path, body: &str) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn test_config(workspace: PathBuf) -> Arc<crate::config::Config> {
        Arc::new(crate::config::Config {
            workspace_dir: workspace,
            ..crate::config::Config::default()
        })
    }

    #[tokio::test]
    async fn skills_install_rejects_empty_slug() {
        let profile = crate::profile::Profile {
            name: "test".into(),
            root: std::env::temp_dir().join("rantaiclaw_install_tool_test"),
        };
        let tool = SkillsInstallTool::new(profile);
        let result = tool.execute(json!({"slug": ""})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("non-empty"));
    }

    #[tokio::test]
    async fn skills_install_missing_slug_returns_err() {
        let profile = crate::profile::Profile {
            name: "test".into(),
            root: std::env::temp_dir().join("rantaiclaw_install_tool_test_missing"),
        };
        let tool = SkillsInstallTool::new(profile);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn skills_install_deps_unknown_skill_points_at_skills_install() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("skills")).unwrap();
        let config = test_config(workspace.clone());
        let tool = SkillsInstallDepsTool::new(workspace, config);
        let result = tool.execute(json!({"name": "nonexistent"})).await.unwrap();
        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(err.contains("No installed skill"));
        assert!(err.contains("skills_install"));
    }

    #[tokio::test]
    async fn skills_install_deps_no_recipes_returns_clear_error() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        // Skill that's installed but ships no install recipes — gating
        // would otherwise leave the user stuck.
        write(
            &workspace.join("skills/greeter/SKILL.md"),
            "---\nname: greeter\ndescription: hi\nversion: 1.0\n---\n# Greeter\n",
        );
        let config = test_config(workspace.clone());
        let tool = SkillsInstallDepsTool::new(workspace, config);
        let result = tool.execute(json!({"name": "greeter"})).await.unwrap();
        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(err.contains("no install recipes"));
    }

    #[test]
    fn install_tools_have_stable_names_and_advertise_approved_arg() {
        let profile = crate::profile::Profile {
            name: "t".into(),
            root: std::env::temp_dir().join("rantaiclaw_install_names"),
        };
        let install = SkillsInstallTool::new(profile);
        assert_eq!(install.name(), "skills_install");
        let schema = install.parameters_schema();
        assert!(schema["properties"]["approved"].is_object());

        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path().to_path_buf());
        let deps = SkillsInstallDepsTool::new(tmp.path().to_path_buf(), config);
        assert_eq!(deps.name(), "skills_install_deps");
        let schema2 = deps.parameters_schema();
        assert!(schema2["properties"]["approved"].is_object());
    }
}
