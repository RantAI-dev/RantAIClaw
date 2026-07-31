//! Who put a skill on disk — recorded, not guessed.
//!
//! Skills arrive five ways and three of them land as a byte-identical plain
//! directory holding `SKILL.md`: `author_skill` from chat, a bundled pack, and
//! a third party copying a folder in. Nothing on disk distinguishes them, so
//! any feature that must treat "a skill you wrote" differently from "a skill
//! someone else manages" has had nothing to key on.
//!
//! That matters because editing the wrong one loses work silently: a bundled
//! skill is re-seeded by the next `setup` run, and a vendor-managed one is
//! overwritten by the vendor's next installer.
//!
//! So every write path we control drops an [`ORIGIN_FILE`] beside `SKILL.md`
//! naming itself. This mirrors [`super::clawhub`]'s `.clawhub.json`, which
//! solved the same problem for one source; the two files stay separate because
//! they answer different questions (`.clawhub.json` records *which publisher
//! and version* so `skills update` can re-fetch correctly).
//!
//! **Not a security boundary.** The marker lives in a directory the operator
//! can already write to, so `kind: "authored"` is forgeable — but forging it
//! grants nothing, since anyone who can write the marker can write the skill
//! body directly. It prevents accidents, not tampering.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Marker filename, beside `SKILL.md` at the root of a skill directory.
pub(crate) const ORIGIN_FILE: &str = ".origin.json";

/// Which write path created a skill directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillOriginKind {
    /// Written by the user through `author_skill` or an editor surface.
    /// The only kind that may be edited in place.
    Authored,
    /// Pulled from ClawHub. Also carries a `.clawhub.json`.
    Clawhub,
    /// Seeded from a bundled starter/core pack.
    Bundled,
    /// Cloned from a git remote.
    Git,
    /// Installed from a local path (copy or symlink).
    Local,
}

impl SkillOriginKind {
    /// Parse a `kind` string. Returns `None` for anything unrecognised so a
    /// marker written by a future version degrades to "unknown" rather than to
    /// a wrong answer.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "authored" => Some(Self::Authored),
            "clawhub" => Some(Self::Clawhub),
            "bundled" => Some(Self::Bundled),
            "git" => Some(Self::Git),
            "local" => Some(Self::Local),
            _ => None,
        }
    }
}

/// Contents of [`ORIGIN_FILE`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillOrigin {
    pub kind: SkillOriginKind,
    /// Where the skill came from, when that is meaningful: an install path, a
    /// git URL, or a ClawHub `@owner/slug` reference. `None` for `Authored`
    /// and `Bundled`, which have no external source.
    #[serde(default)]
    pub source: Option<String>,
}

impl SkillOrigin {
    pub(crate) fn new(kind: SkillOriginKind, source: Option<String>) -> Self {
        Self { kind, source }
    }
}

/// Write the marker into `dir`.
///
/// Callers treat failure as non-fatal: an install that succeeded but could not
/// record its origin degrades to "unknown", which is safe, whereas rolling an
/// install back because a metadata file would not write is not.
pub(crate) fn write_origin(dir: &Path, origin: &SkillOrigin) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(origin).map_err(std::io::Error::other)?;
    std::fs::write(dir.join(ORIGIN_FILE), json)
}

/// Read the marker from `dir`, or `None` when it is missing, unreadable,
/// malformed, or names a `kind` this build does not know.
pub(crate) fn read_origin(dir: &Path) -> Option<SkillOrigin> {
    let raw = std::fs::read_to_string(dir.join(ORIGIN_FILE)).ok()?;
    // Deserialize `kind` as a plain string and map it ourselves rather than
    // deriving straight into the enum: serde would reject an unknown variant
    // as an error, and we want the whole marker to read as "unknown origin"
    // instead of failing.
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let kind = SkillOriginKind::parse(value.get("kind")?.as_str()?)?;
    let source = value
        .get("source")
        .and_then(|s| s.as_str())
        .map(str::to_string);
    Some(SkillOrigin::new(kind, source))
}

/// Resolve a skill directory's origin: the marker if present, otherwise an
/// inference from the directory's shape.
///
/// Every skill that predates the marker has none — including ones the user
/// genuinely wrote by hand, which is how skills were made until now. Treating
/// all of them as "not yours" would mean the Edit affordance never appears for
/// exactly the skills it exists for. So when no marker is present we infer
/// [`SkillOriginKind::Authored`] from four conditions that together describe
/// "a skill directory this install owns and nothing else claims":
///
/// * a real directory, not a symlink (a symlink is a local-path install)
/// * directly under `profile_skills_dir` (the root our own write paths use)
/// * no `.clawhub.json` beside it (that would make it a ClawHub install)
/// * slug not in the bundled packs (those are re-seeded by `setup`)
///
/// A present marker always wins, including one that contradicts the shape.
///
/// The inference is transitional: the first save through any editor writes a
/// real marker, after which that skill is never shape-inferred again.
///
/// Open-skills entries need no special case. They are flat `.md` files sitting
/// directly in the open-skills checkout, so `dir` is that checkout — which
/// fails the `profile_skills_dir` test and correctly resolves to `None`.
pub(crate) fn resolve_origin(dir: &Path, profile_skills_dir: Option<&Path>) -> Option<SkillOrigin> {
    if let Some(marker) = read_origin(dir) {
        return Some(marker);
    }

    // Symlinked directory → a local-path install, not something we authored.
    if std::fs::symlink_metadata(dir)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(true)
    {
        return None;
    }

    if dir.join(super::clawhub::PROVENANCE_FILE).exists() {
        return None;
    }

    let slug = dir.file_name()?.to_str()?;
    let is_bundled = super::bundled::CORE_PACK
        .iter()
        .chain(super::bundled::STARTER_PACK.iter())
        .any(|entry| entry.slug.eq_ignore_ascii_case(slug));
    if is_bundled {
        return None;
    }

    // Canonicalize both sides: comparing raw paths would let a symlinked or
    // `..`-containing parent masquerade as the profile skills root.
    let parent = dir.parent()?.canonicalize().ok()?;
    let root = profile_skills_dir?.canonicalize().ok()?;
    if parent != root {
        return None;
    }

    Some(SkillOrigin::new(SkillOriginKind::Authored, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn skill_dir(root: &Path, slug: &str) -> std::path::PathBuf {
        let dir = root.join(slug);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "---\nname: x\n---\n").unwrap();
        dir
    }

    #[test]
    fn origin_round_trips_every_kind() {
        let tmp = tempfile::tempdir().unwrap();
        for (kind, source) in [
            (SkillOriginKind::Authored, None),
            (SkillOriginKind::Clawhub, Some("@owner/slug".to_string())),
            (SkillOriginKind::Bundled, None),
            (
                SkillOriginKind::Git,
                Some("https://example.com/x".to_string()),
            ),
            (SkillOriginKind::Local, Some("/tmp/x".to_string())),
        ] {
            let origin = SkillOrigin::new(kind, source.clone());
            write_origin(tmp.path(), &origin).unwrap();
            assert_eq!(read_origin(tmp.path()), Some(origin));
        }
    }

    #[test]
    fn missing_marker_reads_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_origin(tmp.path()), None);
    }

    #[test]
    fn malformed_marker_reads_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(ORIGIN_FILE), "{not json").unwrap();
        assert_eq!(read_origin(tmp.path()), None);
    }

    #[test]
    fn unknown_kind_reads_as_none_not_as_a_wrong_kind() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join(ORIGIN_FILE),
            r#"{"kind":"kind-from-the-future","source":null}"#,
        )
        .unwrap();
        assert_eq!(read_origin(tmp.path()), None);
    }

    #[test]
    fn marker_wins_over_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        let dir = skill_dir(&root, "looks-authored");
        // Shape says Authored; the marker says otherwise and must win.
        write_origin(&dir, &SkillOrigin::new(SkillOriginKind::Clawhub, None)).unwrap();
        assert_eq!(
            resolve_origin(&dir, Some(&root)).map(|o| o.kind),
            Some(SkillOriginKind::Clawhub)
        );
    }

    #[test]
    fn plain_dir_in_profile_root_infers_authored() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        let dir = skill_dir(&root, "handmade");
        assert_eq!(
            resolve_origin(&dir, Some(&root)).map(|o| o.kind),
            Some(SkillOriginKind::Authored)
        );
    }

    #[test]
    fn clawhub_provenance_blocks_inference() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        let dir = skill_dir(&root, "weather");
        fs::write(dir.join(super::super::clawhub::PROVENANCE_FILE), "{}").unwrap();
        assert_eq!(resolve_origin(&dir, Some(&root)), None);
    }

    #[test]
    fn bundled_slug_blocks_inference() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        let slug = super::super::bundled::CORE_PACK[0].slug;
        let dir = skill_dir(&root, slug);
        assert_eq!(resolve_origin(&dir, Some(&root)), None);
    }

    #[test]
    fn dir_outside_profile_root_does_not_infer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();
        let elsewhere = tmp.path().join("workspace").join("skills");
        let dir = skill_dir(&elsewhere, "vendor-drop");
        assert_eq!(resolve_origin(&dir, Some(&root)), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dir_does_not_infer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        fs::create_dir_all(&root).unwrap();
        let real = skill_dir(&tmp.path().join("elsewhere"), "dev-skill");
        let link = root.join("dev-skill");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(resolve_origin(&link, Some(&root)), None);
    }

    #[test]
    fn no_profile_root_does_not_infer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("skills");
        let dir = skill_dir(&root, "handmade");
        assert_eq!(resolve_origin(&dir, None), None);
    }
}
