//! Where a skill's own tools live, and how a required binary is found.
//!
//! An install recipe writes into a directory that belongs to that skill
//! alone: `<XDG data>/rantaiclaw/tools/<skill name>/`. Nothing there is on
//! the operator's `PATH`, so every "is the binary there" decision has to know
//! about it. A lookup that only knew about `PATH` reported a successful
//! recipe as a failure ("recipe ran but `<bin>` still missing") and left the
//! skill gated forever.
//!
//! The writer ([`resolve_target_dir`]) and the reader
//! ([`resolve_required_bin`], [`skill_tool_dirs`]) live in the same module on
//! purpose: they must never drift apart, or a recipe writes somewhere the
//! reader does not look.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Result};

use super::Skill;

/// The base directory a skill's own tools live in:
/// `<XDG data>/rantaiclaw/tools/<skill name>/`.
///
/// The key is the skill's manifest **name**, not its directory slug: the
/// recipe runner has always written here under the name, and moving the key
/// would strand every binary already installed.
///
/// `None` when the name cannot be used as one path component — see
/// [`is_plain_component`]. The name comes from the manifest and is free
/// text, so it can carry a separator, a `..`, a `.`, a NUL byte or a `:`,
/// and `Path::join` would treat that as structure: the tools directory
/// would land outside `<data>/tools/`. A drive prefix ("C:foo") is the
/// Windows case of a `:`, but a colon is refused on every platform. A
/// skill without a valid key has no tools directory of its own.
///
/// When the data directory cannot be resolved the fallback is
/// `.rantaiclaw/tools/<skill name>`, relative to the current directory.
pub(crate) fn tools_base_dir(skill_name: &str) -> Option<PathBuf> {
    if !is_plain_component(skill_name) {
        return None;
    }
    Some(
        directories::ProjectDirs::from("", "", "rantaiclaw")
            .map(|d| d.data_dir().join("tools").join(skill_name))
            .unwrap_or_else(|| PathBuf::from(format!(".rantaiclaw/tools/{skill_name}"))),
    )
}

/// Whether a string can be used as exactly one path component.
///
/// Both the skill name and a required binary name come from a manifest,
/// which is free text. `Path::join` treats a separator, a `..`, a NUL byte
/// and a Windows drive prefix as structure rather than as a name, so any of
/// them would put the path outside the directory it was meant to stay in.
///
/// The rule is the same on every platform, so it can be tested on every
/// platform: a `:` is refused everywhere, not only on Windows. A drive
/// prefix is structure only on Windows, but a `:` is not a legal file name
/// there either, and no real skill or binary name carries one.
fn is_plain_component(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return false;
    }
    // A Windows drive prefix ("C:foo") is structure to `Path::join`: it
    // replaces the whole path. Refused on every platform so the rule is
    // the same everywhere and can be tested everywhere.
    if name.contains(':') {
        return false;
    }
    // The string checks above already exclude every name that is not a
    // single component, so this cannot be false as written. It stays
    // because it states the invariant directly: a later change to the
    // string checks cannot then let through a name the platform reads as
    // structure.
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none()
}

/// Resolve the directory a `download` recipe may write into.
///
/// The base is always `<data>/tools/<skill name>/` (the historical default). A
/// recipe MAY narrow to a subdirectory by supplying a **relative**
/// `target_dir` with no parent (`..`) components; an absolute path or any
/// `..` is rejected so an untrusted recipe cannot escape the base (e.g.
/// dropping an executable into `~/.local/bin` to hijack `$PATH`).
///
/// A skill name that cannot be a single path component is rejected too: the
/// recipe must not write outside `<data>/tools/`.
///
/// The writer and the reader of that directory live in this module on
/// purpose, so they cannot drift apart: the reader is
/// [`resolve_required_bin`] and [`skill_tool_dirs`]. A `target_dir` this
/// function would refuse (absolute, or with a `..` component) is one the
/// reader never looks in.
pub(crate) fn resolve_target_dir(target_dir: Option<&str>, skill_name: &str) -> Result<PathBuf> {
    let Some(base) = tools_base_dir(skill_name) else {
        bail!(
            "skill name `{skill_name}` cannot be used as a directory key: \
             it must be a single path component"
        );
    };

    let Some(raw) = target_dir else {
        return Ok(base);
    };
    let rel = Path::new(raw);
    if rel.is_absolute() {
        bail!("download recipe target_dir must be relative, got absolute `{raw}`");
    }
    for comp in rel.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => bail!("download recipe target_dir `{raw}` has a forbidden path component"),
        }
    }
    Ok(base.join(rel))
}

/// The directories that belong to one skill: its base tools directory plus,
/// for every recipe that narrows `target_dir`, the narrowed subdirectory.
///
/// A `target_dir` the writer would refuse (absolute, or with a `..`
/// component) is skipped rather than added — the reader must never look
/// somewhere the writer would not write.
///
/// A skill whose name is not a single path component has no directory at
/// all: that is the whole isolation guarantee — no directory, no `PATH`
/// entry, nothing to reach.
pub(crate) fn skill_tool_dirs(skill: &Skill) -> Vec<PathBuf> {
    let Some(base) = tools_base_dir(&skill.name) else {
        return Vec::new();
    };
    let mut dirs = vec![base.clone()];
    for recipe in &skill.install_recipes {
        let Some(raw) = recipe.target_dir.as_deref() else {
            continue;
        };
        let Ok(dir) = resolve_target_dir(Some(raw), &skill.name) else {
            continue;
        };
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Find a binary a skill requires: the skill's own tools directories first,
/// then the process `PATH`.
///
/// Inside a skill directory a candidate counts as present only when it is a
/// regular file, is executable, and is not a dangling symlink.
/// `fs::metadata` follows symlinks, so a dangling link reports `NotFound` and
/// a directory fails `is_file()`.
pub(crate) fn resolve_required_bin(skill: &Skill, bin: &str) -> Option<PathBuf> {
    for dir in skill_tool_dirs(skill) {
        if let Some(found) = executable_in(&dir, bin) {
            return Some(found);
        }
    }
    which::which(bin).ok()
}

/// Whether `<dir>/<bin>` is a file that can be executed.
///
/// The binary name has to be a plain file name: a name that carries a
/// separator or a Windows drive prefix would leave the skill's own
/// directory, where another skill's binary lives. The `PATH` fallback is a
/// separate lookup and keeps its own behaviour.
fn executable_in(dir: &Path, bin: &str) -> Option<PathBuf> {
    if !is_plain_component(bin) {
        return None;
    }
    let candidate = dir.join(bin);
    let meta = std::fs::metadata(&candidate).ok()?;
    if !meta.is_file() {
        return None;
    }
    // Unix only: the execute bit says the file can be run. Windows has no
    // such bit, so there every regular file counts as present. The
    // limitation is stated here rather than worked around.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    Some(candidate)
}

/// The `PATH` a skill's own tool runs with: the skill's tools directories
/// first, then the parent's `PATH`.
///
/// The skill's own binary has to win over a same-named binary on the parent's
/// `PATH`, so the directories are prepended rather than appended. Only the
/// child that runs this skill's tool sees this value — the process-wide
/// `PATH` is never changed, and no other skill's directory is included.
pub(crate) fn skill_path_env(skill: &Skill) -> OsString {
    let mut paths = skill_tool_dirs(skill);
    if let Some(parent) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&parent));
    }
    std::env::join_paths(paths).unwrap_or_else(|_| {
        // A path the platform separator cannot express. Keep the parent's
        // PATH rather than hand the child a broken one — and say so, so a
        // skill whose own binary cannot be found has an explanation.
        tracing::warn!(
            skill = %skill.name,
            "the skill's tools directory could not be put on the child's PATH; \
             the child runs with the parent's PATH"
        );
        std::env::var_os("PATH").unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{SkillInstallRecipe, SkillRequires};

    fn skill_with(name: &str, recipes: Vec<SkillInstallRecipe>) -> Skill {
        Skill {
            name: name.to_string(),
            description: "A skill".to_string(),
            version: "0.1.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec![],
            location: None,
            requires: SkillRequires::default(),
            install_recipes: recipes,
            remote: false,
            origin: None,
        }
    }

    /// A recipe that narrows the target directory.
    fn download_recipe(target_dir: Option<&str>) -> SkillInstallRecipe {
        SkillInstallRecipe {
            kind: "download".to_string(),
            target_dir: target_dir.map(str::to_string),
            ..Default::default()
        }
    }

    /// The base directory of a skill, under a temporary data home.
    fn with_data_home<R>(f: impl FnOnce(&Path) -> R) -> R {
        let data = tempfile::tempdir().expect("tempdir");
        with_data_home_at(data.path(), f)
    }

    /// The same, under a data home the caller chose. A directory name may
    /// hold a `:` — legal on disk, impossible in a `PATH` entry — which is
    /// how a test reaches the branch where the tools directory cannot be
    /// put on the child's `PATH`.
    fn with_data_home_at<R>(data_home: &Path, f: impl FnOnce(&Path) -> R) -> R {
        let guard = crate::test_env::EnvGuard::set("XDG_DATA_HOME", data_home);
        let result = f(data_home);
        drop(guard);
        result
    }

    /// The WARN lines emitted while `run` executes. A scoped subscriber, so
    /// parallel tests cannot see each other's events.
    fn warnings_from(run: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Buffer {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .lock()
                    .expect("lock the log buffer")
                    .extend_from_slice(buf);
                Ok(buf.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        impl tracing_subscriber::fmt::MakeWriter<'_> for Buffer {
            type Writer = Self;

            fn make_writer(&self) -> Self {
                self.clone()
            }
        }

        let buffer = Buffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        let bytes = buffer.0.lock().expect("lock the log buffer").clone();
        String::from_utf8(bytes).expect("log output is utf-8")
    }

    /// Write an executable file and return its path.
    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
        std::fs::write(path, body).expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// The base directory of a skill whose name is a plain path component.
    fn base_dir(skill_name: &str) -> PathBuf {
        tools_base_dir(skill_name).expect("a plain skill name has a base directory")
    }

    /// A directory is not a binary, even when it carries the right name.
    #[test]
    fn a_directory_is_not_a_present_binary() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|data| {
            let skill = skill_with("dir-skill", vec![]);
            let dir = base_dir("dir-skill");
            std::fs::create_dir_all(dir.join("not-a-binary")).expect("create dir");
            assert!(
                resolve_required_bin(&skill, "not-a-binary").is_none(),
                "a directory must not count as a present binary ({} exists)",
                data.join("rantaiclaw/tools/dir-skill/not-a-binary")
                    .display()
            );
        });
    }

    /// A file without an execute bit is not runnable, so it is not present.
    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_is_not_a_present_binary() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with("plain-skill", vec![]);
            let dir = base_dir("plain-skill");
            std::fs::create_dir_all(&dir).expect("create dir");
            std::fs::write(dir.join("not-executable"), "#!/bin/sh\n").expect("write");
            assert!(
                resolve_required_bin(&skill, "not-executable").is_none(),
                "a file without an execute bit must not count as a present binary"
            );
        });
    }

    /// A symlink that points nowhere must not count as a present binary.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_not_a_present_binary() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with("link-skill", vec![]);
            let dir = base_dir("link-skill");
            std::fs::create_dir_all(&dir).expect("create dir");
            std::os::unix::fs::symlink(dir.join("does-not-exist"), dir.join("dangling"))
                .expect("link");
            assert!(
                resolve_required_bin(&skill, "dangling").is_none(),
                "a dangling symlink must not count as a present binary"
            );
        });
    }

    /// An executable in the skill's own directory is found.
    #[cfg(unix)]
    #[test]
    fn an_executable_in_the_skill_directory_is_found() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with("own-skill", vec![]);
            let dir = base_dir("own-skill");
            write_executable(&dir.join("own-bin"), "#!/bin/sh\n");
            assert_eq!(
                resolve_required_bin(&skill, "own-bin"),
                Some(dir.join("own-bin")),
                "an executable in the skill's own directory must be found"
            );
        });
    }

    /// A recipe that narrows the target directory puts the binary in a
    /// subdirectory, and the reader has to look there too.
    #[cfg(unix)]
    #[test]
    fn a_narrowed_target_directory_is_searched_as_well() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with("narrow-skill", vec![download_recipe(Some("bin"))]);
            let dir = base_dir("narrow-skill").join("bin");
            write_executable(&dir.join("narrow-bin"), "#!/bin/sh\n");
            assert_eq!(
                resolve_required_bin(&skill, "narrow-bin"),
                Some(dir.join("narrow-bin")),
                "a binary in the narrowed target directory must be found"
            );
        });
    }

    /// A `target_dir` the writer would refuse is never searched.
    #[test]
    fn a_forbidden_target_directory_is_never_searched() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with("escape-skill", vec![download_recipe(Some("../elsewhere"))]);
            let dirs = skill_tool_dirs(&skill);
            assert_eq!(
                dirs,
                vec![base_dir("escape-skill")],
                "a target_dir with a `..` component must not be added"
            );
        });
    }

    /// The skill's own binary wins over a same-named binary on the parent's
    /// `PATH`.
    #[test]
    fn the_skill_directory_comes_first_on_the_path() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with("path-skill", vec![]);
            let path = skill_path_env(&skill);
            let entries: Vec<PathBuf> = std::env::split_paths(&path).collect();
            assert_eq!(
                entries.first(),
                Some(&base_dir("path-skill")),
                "the skill's own directory must come first on the child's PATH"
            );
        });
    }

    /// A tools directory whose path the platform separator cannot express
    /// is not put on the child's `PATH`: the child runs with the parent's
    /// `PATH` instead, and the operator is told why, so a skill whose own
    /// binary cannot be found has an explanation.
    ///
    /// Unix only: a `:` is legal in a directory name there and impossible
    /// in a `PATH` entry, which is what makes the join fail. Windows uses
    /// `;` as its separator and forbids a `:` in a name altogether.
    #[cfg(unix)]
    #[test]
    fn a_tools_directory_the_path_separator_cannot_express_is_reported() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let root = tempfile::tempdir().expect("temp dir");
        let data_home = root.path().join("colon:dir");
        std::fs::create_dir_all(&data_home).expect("create data home");

        let skill = skill_with("colon-skill", vec![]);
        let (path, parent, logged) = with_data_home_at(&data_home, |_| {
            let mut path = None;
            let logged = warnings_from(|| {
                path = Some(skill_path_env(&skill));
            });
            (
                path.expect("the child's PATH was computed"),
                std::env::var_os("PATH").unwrap_or_default(),
                logged,
            )
        });

        assert_eq!(
            path, parent,
            "the child must run with the parent's PATH when the skill's own directory cannot be put on it"
        );
        assert!(
            logged.contains("colon-skill"),
            "the warning must name the skill: {logged}"
        );
    }

    /// A skill name that carries a path separator is not a directory key.
    /// `victim/../victim` would resolve to ANOTHER skill's own tools
    /// directory, which would break the isolation between skills.
    #[cfg(unix)]
    #[test]
    fn a_name_with_a_parent_component_reaches_no_tools_directory() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|data| {
            let skill = skill_with("victim/../victim", vec![]);
            assert!(
                skill_tool_dirs(&skill).is_empty(),
                "a name with a `..` component must not resolve to a tools directory"
            );

            // Another skill's binary must not satisfy this skill's own
            // requirement.
            write_executable(
                &data.join("rantaiclaw/tools/victim/victim-bin"),
                "#!/bin/sh\n",
            );
            assert!(
                resolve_required_bin(&skill, "victim-bin").is_none(),
                "another skill's binary must not count as this skill's own"
            );
        });
    }

    /// `.` as a skill name would put the tools directory at the shared
    /// parent `tools/` directory.
    #[test]
    fn a_current_directory_name_reaches_no_tools_directory() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with(".", vec![]);
            assert!(
                skill_tool_dirs(&skill).is_empty(),
                "`.` must not resolve to a tools directory"
            );
        });
    }

    /// `..` as a skill name would put the tools directory outside
    /// `<data>/tools/` altogether.
    #[test]
    fn a_parent_directory_name_reaches_no_tools_directory() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with("..", vec![]);
            assert!(
                skill_tool_dirs(&skill).is_empty(),
                "`..` must not resolve to a tools directory"
            );
        });
    }

    /// An absolute skill name would put the tools directory outside
    /// `<data>/tools/` entirely.
    #[test]
    fn an_absolute_name_reaches_no_tools_directory() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with("/usr/local/bin", vec![]);
            assert!(
                tools_base_dir("/usr/local/bin").is_none(),
                "an absolute name must not resolve to a base directory"
            );
            assert!(
                skill_tool_dirs(&skill).is_empty(),
                "an absolute name must not resolve to a tools directory"
            );
        });
    }

    /// An empty skill name has no directory of its own.
    #[test]
    fn an_empty_name_reaches_no_tools_directory() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with("", vec![]);
            assert!(
                skill_tool_dirs(&skill).is_empty(),
                "an empty name must not resolve to a tools directory"
            );
        });
    }

    /// A name that is a plain path component stays byte-identical, so an
    /// install that already used it is not stranded.
    #[test]
    fn a_name_with_a_space_or_an_inner_dot_keeps_its_own_directory() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|data| {
            for name in ["my skill", "my.skill"] {
                let skill = skill_with(name, vec![]);
                assert_eq!(
                    skill_tool_dirs(&skill),
                    vec![data.join("rantaiclaw/tools").join(name)],
                    "`{name}` must keep its own directory, byte for byte"
                );
            }
        });
    }

    /// The writer refuses an unsafe skill name: a recipe must not write
    /// outside `<data>/tools/`.
    #[test]
    fn the_writer_refuses_an_unsafe_skill_name() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let err = resolve_target_dir(None, "victim/../victim")
                .expect_err("an unsafe skill name must be refused");
            let message = err.to_string();
            assert!(
                message.contains("victim/../victim"),
                "the error must name the skill: {message}"
            );
        });
    }

    /// A skill whose name is unsafe gets the parent's `PATH` and nothing
    /// else: no directory of its own is added to it.
    #[test]
    fn an_unsafe_name_adds_no_directory_to_the_child_path() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|data| {
            let skill = skill_with("victim/../victim", vec![]);
            let path = skill_path_env(&skill);
            let parent = std::env::var_os("PATH").unwrap_or_default();
            assert_eq!(
                path, parent,
                "a skill without a tools directory must get the parent's PATH"
            );
            let entries: Vec<PathBuf> = std::env::split_paths(&path).collect();
            assert!(
                !entries
                    .iter()
                    .any(|entry| entry.starts_with(data.join("rantaiclaw/tools"))),
                "the child's PATH must carry no tools directory: {entries:?}"
            );
        });
    }

    /// A required binary that is not a plain file name is not looked up in
    /// the skill's own directory: `../escape-bin` would leave it.
    #[cfg(unix)]
    #[test]
    fn a_bin_that_is_not_a_plain_file_name_is_not_found_in_the_skill_directory() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|data| {
            let mut skill = skill_with("escape-bin-skill", vec![]);
            skill.requires.bins = vec!["../escape-bin".to_string()];

            // One level above the skill's own tools directory, which has to
            // exist for the `..` to resolve at all.
            let own = data.join("rantaiclaw/tools/escape-bin-skill");
            std::fs::create_dir_all(&own).expect("create dir");
            write_executable(&data.join("rantaiclaw/tools/escape-bin"), "#!/bin/sh\n");

            // The `PATH` fallback resolves a relative name against the
            // current directory, so the probe only means what it says while
            // no such file exists there.
            assert!(
                which::which("../escape-bin").is_err(),
                "the probe file must not exist relative to the current directory"
            );

            assert!(
                resolve_required_bin(&skill, "../escape-bin").is_none(),
                "a bin that leaves the skill's directory must not be found there"
            );

            let reasons = skill.requires.unmet(&skill);
            assert!(
                reasons.iter().any(|r| r.contains("`../escape-bin`")),
                "the requirement must still be reported as missing: {reasons:?}"
            );
        });
    }

    /// A `target_dir` the writer would refuse is never searched: an absolute
    /// path is refused just like a `..` component.
    #[test]
    fn an_absolute_target_directory_is_never_searched() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|_| {
            let skill = skill_with("absolute-target-skill", vec![download_recipe(Some("/etc"))]);
            let dirs = skill_tool_dirs(&skill);
            assert_eq!(
                dirs,
                vec![base_dir("absolute-target-skill")],
                "an absolute target_dir must not be added"
            );
        });
    }

    /// Only a plain path component is a usable name. A Windows drive prefix
    /// ("C:foo") is structure to `Path::join` there: it replaces the path
    /// instead of extending it, so the tools directory would land outside
    /// `<data>/tools/`. The rule is the same on every platform, so it can be
    /// tested on every platform.
    #[test]
    fn only_a_plain_path_component_is_accepted() {
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "C:foo",
            "C:..",
            "node:js",
            "nul\0name",
        ] {
            assert!(
                !is_plain_component(name),
                "`{name}` must not be accepted as a path component"
            );
        }
        for name in ["my.skill", "a b", "ünïcode", "C"] {
            assert!(
                is_plain_component(name),
                "`{name}` must be accepted as a path component"
            );
        }
    }

    /// A skill name carrying a Windows drive prefix is not a directory key:
    /// it gets no tools directory, the writer refuses it, and the child's
    /// `PATH` stays the parent's.
    #[test]
    fn a_drive_prefix_name_reaches_no_tools_directory() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|data| {
            let skill = skill_with("C:foo", vec![]);
            assert!(
                tools_base_dir("C:foo").is_none(),
                "a drive prefix must not resolve to a base directory"
            );
            assert!(
                skill_tool_dirs(&skill).is_empty(),
                "a drive prefix must not resolve to a tools directory"
            );

            let err = resolve_target_dir(None, "C:foo")
                .expect_err("the writer must refuse a drive prefix");
            assert!(
                err.to_string().contains("C:foo"),
                "the error must name the skill: {err}"
            );

            let path = skill_path_env(&skill);
            let parent = std::env::var_os("PATH").unwrap_or_default();
            assert_eq!(
                path, parent,
                "a skill without a tools directory must get the parent's PATH"
            );
            let entries: Vec<PathBuf> = std::env::split_paths(&path).collect();
            assert!(
                !entries
                    .iter()
                    .any(|entry| entry.starts_with(data.join("rantaiclaw/tools"))),
                "the child's PATH must carry no tools directory: {entries:?}"
            );
        });
    }

    /// A skill whose name carries a drive prefix reaches no binary, not even
    /// one that happens to sit where the prefix would have put its tools
    /// directory.
    #[cfg(unix)]
    #[test]
    fn a_drive_prefix_name_reaches_no_binary() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|data| {
            let skill = skill_with("C:foo", vec![]);
            write_executable(
                &data.join("rantaiclaw/tools/C:foo/drive-prefix-bin"),
                "#!/bin/sh\n",
            );
            assert!(
                resolve_required_bin(&skill, "drive-prefix-bin").is_none(),
                "a skill without a tools directory must not reach one"
            );
        });
    }

    /// A binary name carrying a drive prefix is not looked up in the skill's
    /// own directory: `dir.join("C:foo")` is structure on Windows, so the
    /// lookup would resolve against the current directory instead of the
    /// skill's own.
    #[cfg(unix)]
    #[test]
    fn a_drive_prefix_bin_is_not_found_in_the_skill_directory() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        with_data_home(|data| {
            let skill = skill_with("drive-prefix-bin-skill", vec![]);
            let own = base_dir("drive-prefix-bin-skill");

            // One level above the skill's own tools directory, and inside
            // it: neither may satisfy the requirement.
            write_executable(&data.join("rantaiclaw/tools/C:foo"), "#!/bin/sh\n");
            write_executable(&own.join("C:foo"), "#!/bin/sh\n");

            assert!(
                resolve_required_bin(&skill, "C:foo").is_none(),
                "a bin that carries a drive prefix must not be found"
            );
        });
    }

    /// The `PATH` fallback is a separate lookup and keeps its own behaviour:
    /// a name with a `:` in it is still found on the `PATH`.
    #[cfg(unix)]
    #[test]
    fn a_drive_prefix_bin_is_still_found_on_the_path() {
        let _env = crate::test_env::ENV_LOCK.blocking_lock();
        let path_dir = tempfile::tempdir().expect("temp dir");
        write_executable(&path_dir.path().join("C:foo"), "#!/bin/sh\n");
        let mut entries = vec![path_dir.path().to_path_buf()];
        entries.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let _path = crate::test_env::EnvGuard::set(
            "PATH",
            std::env::join_paths(entries).expect("join paths"),
        );

        let skill = skill_with("path-fallback-skill", vec![]);
        // The skill's own tools directory sits under the temporary data
        // home, so the test never reads the operator's real one. It holds
        // no `C:foo`, and a name carrying a colon is refused there, so the
        // binary below was found on the `PATH` and nowhere else.
        with_data_home(|_| {
            assert_eq!(
                resolve_required_bin(&skill, "C:foo"),
                Some(path_dir.path().join("C:foo")),
                "the PATH fallback must keep its own behaviour"
            );
        });
    }
}
