//! Read-only skill discovery tools — Hermes-parity for the
//! "agent helps me find a skill" flow.
//!
//! Three tools registered for the LLM:
//!
//! - `skills_list` → all installed skills (active + gated) as JSON.
//!   Lets the agent know about gated skills and tell the user how to
//!   un-gate them. Closes the coherence gap where `/skills` showed
//!   gated rows but the agent's prompt didn't.
//! - `skill_view` → full SKILL.md content + metadata for one skill.
//!   Progressive disclosure pattern (Hermes' `skill_view` analog).
//! - `skills_search` → ClawHub remote search by query. Returns slug +
//!   summary so the agent can suggest installing one.
//!
//! All three are read-only. No approval required. The
//! [`super::skills_install`] sibling module wires the write side.
//!
//! Wired into the registry via `src/tools/mod.rs::all_tools_with_runtime`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use super::traits::{Tool, ToolResult};

/// `skills_list` — return every installed skill (active + gated) with
/// metadata so the agent can suggest one without needing the user to
/// run `/skills` themselves.
pub struct SkillsListTool {
    workspace_dir: PathBuf,
    config: Arc<crate::config::Config>,
}

impl SkillsListTool {
    pub fn new(workspace_dir: PathBuf, config: Arc<crate::config::Config>) -> Self {
        Self {
            workspace_dir,
            config,
        }
    }
}

#[async_trait]
impl Tool for SkillsListTool {
    fn name(&self) -> &str {
        "skills_list"
    }

    fn description(&self) -> &str {
        "List every installed skill (active and gated) with name, version, \
         description, and gating reason. Use this to know what skills the \
         user has and to surface install-deps hints when a gated skill is \
         relevant."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let with_status = crate::skills::load_skills_with_status(&self.workspace_dir, &self.config);
        let mut out = Vec::with_capacity(with_status.len());
        for (skill, reasons) in with_status {
            let active = reasons.is_empty();
            let install_hint = if !active && !skill.install_recipes.is_empty() {
                Some(format!("rantaiclaw skills install-deps {}", skill.name))
            } else {
                None
            };
            out.push(json!({
                "name": skill.name,
                "version": skill.version,
                "description": skill.description,
                "tags": skill.tags,
                "active": active,
                "gated_reasons": if active { Vec::new() } else { reasons },
                "install_hint": install_hint,
            }));
        }
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&out).unwrap_or_else(|_| "[]".into()),
            error: None,
        })
    }
}

/// `skill_view` — return the full SKILL.md body + metadata for one
/// skill. Hermes-parity Level 1 disclosure: agent only loads when it's
/// actually going to use the skill.
pub struct SkillViewTool {
    workspace_dir: PathBuf,
    config: Arc<crate::config::Config>,
}

impl SkillViewTool {
    pub fn new(workspace_dir: PathBuf, config: Arc<crate::config::Config>) -> Self {
        Self {
            workspace_dir,
            config,
        }
    }
}

#[async_trait]
impl Tool for SkillViewTool {
    fn name(&self) -> &str {
        "skill_view"
    }

    fn description(&self) -> &str {
        "Return the full SKILL.md content plus metadata for one installed \
         skill. Pass the skill's `name` (case-insensitive). Use this when \
         the user asks how a specific skill works."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name (case-insensitive)"
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

        // Use status-aware load so we can surface gated skills too —
        // the user might be asking about one they need to install-deps
        // first.
        let with_status = crate::skills::load_skills_with_status(&self.workspace_dir, &self.config);
        let found = with_status
            .into_iter()
            .find(|(s, _)| s.name.eq_ignore_ascii_case(name));

        match found {
            Some((skill, reasons)) => {
                let body = match skill.location.as_ref() {
                    Some(path) => std::fs::read_to_string(path).unwrap_or_default(),
                    None => skill.prompts.join("\n"),
                };
                let payload = json!({
                    "name": skill.name,
                    "version": skill.version,
                    "description": skill.description,
                    "tags": skill.tags,
                    "active": reasons.is_empty(),
                    "gated_reasons": reasons,
                    "install_recipes": skill.install_recipes.iter().map(|r| json!({
                        "kind": r.kind,
                        "label": r.label,
                        "bins": r.bins,
                    })).collect::<Vec<_>>(),
                    "skill_md": body,
                });
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into()),
                    error: None,
                })
            }
            None => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "No skill named `{name}`. Call `skills_list` to see what's installed."
                )),
            }),
        }
    }
}

/// `skills_search` — query ClawHub remote registry. Returns slug +
/// summary for each match. Read-only; install side lives in
/// [`super::skills_install::SkillsInstallTool`].
pub struct SkillsSearchTool;

impl SkillsSearchTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SkillsSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SkillsSearchTool {
    fn name(&self) -> &str {
        "skills_search"
    }

    fn description(&self) -> &str {
        "Search ClawHub (the public registry) for a skill matching the given \
         query. Returns up to 20 matches with slug + summary + star count. \
         Use this when the user wants a capability we don't already have a \
         local skill for; pair with `skills_install` to actually install one."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query (free text). Empty returns top-by-stars."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();

        let results = if query.is_empty() {
            crate::skills::clawhub::list_top(20).await
        } else {
            crate::skills::clawhub::search(query).await
        };

        match results {
            Ok(skills) => {
                let mapped: Vec<_> = skills
                    .into_iter()
                    .map(|s| {
                        json!({
                            "slug": s.slug,
                            "display_name": s.display_name,
                            "summary": s.summary,
                            "stars": s.stats.stars,
                        })
                    })
                    .collect();
                Ok(ToolResult {
                    success: true,
                    output: serde_json::to_string_pretty(&mapped).unwrap_or_else(|_| "[]".into()),
                    error: None,
                })
            }
            Err(e) => Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("ClawHub search failed: {e:#}")),
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
    async fn skills_list_includes_active_and_gated_with_install_hints() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let skills_dir = workspace.join("skills");

        // Active skill.
        write(
            &skills_dir.join("greeter/SKILL.md"),
            "---\nname: greeter\ndescription: Says hi\nversion: 1.0\n---\n# Greeter\n",
        );
        // Gated skill — declares a binary that doesn't exist on this host.
        write(
            &skills_dir.join("vapor/SKILL.md"),
            "---\nname: vapor\ndescription: Needs the vapor CLI\nversion: 1.0\nmetadata: {\"clawdbot\":{\"requires\":{\"bins\":[\"definitely-not-on-path-vapor\"]},\"install\":[{\"id\":\"brew\",\"kind\":\"brew\",\"formula\":\"vapor\",\"bins\":[\"definitely-not-on-path-vapor\"]}]}}\n---\n# Vapor\n",
        );

        let config = test_config(workspace.clone());
        let tool = SkillsListTool::new(workspace.clone(), config);
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result.output).unwrap();
        let by_name: std::collections::HashMap<String, &serde_json::Value> = parsed
            .iter()
            .filter_map(|s| {
                s.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| (n.to_string(), s))
            })
            .collect();
        assert_eq!(
            by_name.get("greeter").and_then(|s| s.get("active")),
            Some(&json!(true))
        );
        assert_eq!(
            by_name.get("vapor").and_then(|s| s.get("active")),
            Some(&json!(false))
        );
        let vapor_hint = by_name
            .get("vapor")
            .and_then(|s| s.get("install_hint"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            vapor_hint.contains("install-deps vapor"),
            "expected install-deps hint, got: {vapor_hint}"
        );
    }

    #[tokio::test]
    async fn skill_view_returns_full_md_for_known_skill() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        write(
            &workspace.join("skills/greeter/SKILL.md"),
            "---\nname: greeter\ndescription: Says hi\nversion: 1.0\n---\n# Greeter body\n",
        );

        let config = test_config(workspace.clone());
        let tool = SkillViewTool::new(workspace, config);
        let result = tool.execute(json!({"name": "greeter"})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("Greeter body"));
        assert!(result.output.contains("\"name\""));
        assert!(result.output.contains("\"active\""));
    }

    #[tokio::test]
    async fn skill_view_unknown_returns_error_pointing_to_skills_list() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("skills")).unwrap();
        let config = test_config(workspace.clone());
        let tool = SkillViewTool::new(workspace, config);
        let result = tool.execute(json!({"name": "nope"})).await.unwrap();
        assert!(!result.success);
        let err = result.error.as_deref().unwrap_or("");
        assert!(err.contains("No skill"));
        assert!(err.contains("skills_list"));
    }

    #[tokio::test]
    async fn skill_view_missing_name_returns_err() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path().to_path_buf());
        let tool = SkillViewTool::new(tmp.path().to_path_buf(), config);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn tools_have_stable_names() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path().to_path_buf());
        assert_eq!(
            SkillsListTool::new(tmp.path().to_path_buf(), config.clone()).name(),
            "skills_list"
        );
        assert_eq!(
            SkillViewTool::new(tmp.path().to_path_buf(), config).name(),
            "skill_view"
        );
        assert_eq!(SkillsSearchTool::new().name(), "skills_search");
    }
}
