use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

/// Watches local skill directories and emits debounced reload ticks.
pub struct SkillsWatcher {
    _watcher: notify::RecommendedWatcher,
    pub reload_rx: mpsc::UnboundedReceiver<()>,
}

impl SkillsWatcher {
    pub fn watch(profile_skills: &Path, workspace_skills: &Path) -> Result<Self> {
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<notify::Event>();
        let (reload_tx, reload_rx) = mpsc::unbounded_channel::<()>();

        if !profile_skills.exists() {
            std::fs::create_dir_all(profile_skills)?;
        }

        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    let _ = raw_tx.send(event);
                }
            })?;

        watcher.watch(profile_skills, RecursiveMode::Recursive)?;
        if workspace_skills != profile_skills && workspace_skills.exists() {
            watcher.watch(workspace_skills, RecursiveMode::Recursive)?;
        }

        tokio::spawn(async move {
            while raw_rx.recv().await.is_some() {
                tokio::time::sleep(Duration::from_millis(500)).await;
                while raw_rx.try_recv().is_ok() {}
                let _ = reload_tx.send(());
            }
        });

        Ok(Self {
            _watcher: watcher,
            reload_rx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emits_reload_when_skill_file_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profile_skills = temp.path().join("profile-skills");
        let workspace_skills = temp.path().join("workspace-skills");
        std::fs::create_dir_all(&workspace_skills).expect("workspace skills dir");

        let mut watcher =
            SkillsWatcher::watch(&profile_skills, &workspace_skills).expect("watcher");

        let skill_dir = profile_skills.join("demo");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(skill_dir.join("SKILL.md"), "# demo\n").expect("write skill");

        tokio::time::timeout(Duration::from_secs(2), watcher.reload_rx.recv())
            .await
            .expect("reload within timeout")
            .expect("reload event");
    }
}
