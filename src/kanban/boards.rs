use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::kanban::errors::{KanbanError, Result};
use crate::kanban::paths::{board_dir, boards_root, current_board_path};

pub const DEFAULT_BOARD: &str = "default";

fn slug_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-z0-9][a-z0-9\-_]{0,63}$").unwrap())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    pub slug: String,
    pub name: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub created_at: i64,
    pub is_default: bool,
}

/// Lower-case + trim a slug; validate format. Returns `None` for empty input,
/// `Err` for malformed slugs.
pub fn normalize_board_slug(slug: Option<&str>) -> Option<String> {
    let raw = slug?.trim().to_lowercase();
    if raw.is_empty() {
        return None;
    }
    if slug_re().is_match(&raw) {
        Some(raw)
    } else {
        None
    }
}

/// Strict version of `normalize_board_slug` that raises on malformed input.
pub fn validate_board_slug(slug: &str) -> Result<String> {
    let raw = slug.trim().to_lowercase();
    if raw.is_empty() {
        return Err(KanbanError::InvalidBoardSlug(slug.to_string()));
    }
    if slug_re().is_match(&raw) {
        Ok(raw)
    } else {
        Err(KanbanError::InvalidBoardSlug(slug.to_string()))
    }
}

/// Return the active board slug, honouring the resolution chain:
///   1. `RANTAICLAW_KANBAN_BOARD` env var.
///   2. `<root>/kanban/current` on disk (when the board exists).
///   3. `default`.
pub fn get_current_board() -> String {
    if let Ok(env) = std::env::var("RANTAICLAW_KANBAN_BOARD") {
        if let Some(slug) = normalize_board_slug(Some(&env)) {
            return slug;
        }
    }
    if let Ok(text) = fs::read_to_string(current_board_path()) {
        let trimmed = text.trim().to_string();
        if let Some(slug) = normalize_board_slug(Some(&trimmed)) {
            if board_exists(Some(&slug)) {
                return slug;
            }
        }
    }
    DEFAULT_BOARD.to_string()
}

/// Persist `slug` as the active board. The caller should validate existence
/// first — this function does not.
pub fn set_current_board(slug: &str) -> Result<PathBuf> {
    let normed = validate_board_slug(slug)?;
    let path = current_board_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, format!("{normed}\n"))?;
    Ok(path)
}

pub fn clear_current_board() -> Result<()> {
    match fs::remove_file(current_board_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// True if the board has a DB or a metadata directory on disk. `default` is
/// always considered to exist (its DB is created on first connect).
pub fn board_exists(board: Option<&str>) -> bool {
    let slug = normalize_board_slug(board).unwrap_or_else(|| DEFAULT_BOARD.to_string());
    if slug == DEFAULT_BOARD {
        return true;
    }
    let dir = board_dir(Some(&slug));
    dir.is_dir() || dir.join("kanban.db").exists()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct BoardMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    created_at: i64,
}

fn board_json_path(slug: &str) -> PathBuf {
    board_dir(Some(slug)).join("board.json")
}

fn read_board_meta(slug: &str) -> Option<BoardMeta> {
    let json = fs::read_to_string(board_json_path(slug)).ok()?;
    serde_json::from_str(&json).ok()
}

fn write_board_meta(slug: &str, meta: &BoardMeta) -> Result<()> {
    let path = board_json_path(slug);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(meta)?;
    fs::write(path, json)?;
    Ok(())
}

pub fn create_board(
    slug: &str,
    name: Option<&str>,
    description: Option<&str>,
    icon: Option<&str>,
) -> Result<Board> {
    let normed = validate_board_slug(slug)?;
    if normed == DEFAULT_BOARD {
        // `default` is always present; treat as a no-op so the CLI is idempotent.
        return Ok(Board {
            slug: DEFAULT_BOARD.to_string(),
            name: "Default".to_string(),
            description: None,
            icon: None,
            created_at: 0,
            is_default: true,
        });
    }
    let dir = board_dir(Some(&normed));
    fs::create_dir_all(dir.join("workspaces"))?;
    fs::create_dir_all(dir.join("logs"))?;
    let meta = BoardMeta {
        name: name.unwrap_or(&normed).to_string(),
        description: description.map(str::to_string),
        icon: icon.map(str::to_string),
        created_at: chrono::Utc::now().timestamp(),
    };
    write_board_meta(&normed, &meta)?;
    Ok(Board {
        slug: normed,
        name: meta.name,
        description: meta.description,
        icon: meta.icon,
        created_at: meta.created_at,
        is_default: false,
    })
}

pub fn rename_board(slug: &str, new_name: &str) -> Result<Board> {
    let normed = validate_board_slug(slug)?;
    if normed == DEFAULT_BOARD {
        return Err(KanbanError::InvalidBoardSlug(slug.to_string()));
    }
    let mut meta = read_board_meta(&normed).unwrap_or_default();
    meta.name = new_name.to_string();
    write_board_meta(&normed, &meta)?;
    Ok(Board {
        slug: normed,
        name: meta.name,
        description: meta.description,
        icon: meta.icon,
        created_at: meta.created_at,
        is_default: false,
    })
}

/// Archive (default) or hard-delete a board. Archiving moves the directory to
/// `<root>/kanban/boards/_archived/<slug>-<timestamp>/`.
pub fn remove_board(slug: &str, hard_delete: bool) -> Result<()> {
    let normed = validate_board_slug(slug)?;
    if normed == DEFAULT_BOARD {
        return Err(KanbanError::InvalidBoardSlug(slug.to_string()));
    }
    let dir = board_dir(Some(&normed));
    if !dir.exists() {
        return Err(KanbanError::UnknownBoard(slug.to_string()));
    }
    if hard_delete {
        fs::remove_dir_all(&dir)?;
    } else {
        let archive_root = boards_root().join("_archived");
        fs::create_dir_all(&archive_root)?;
        let stamp = chrono::Utc::now().timestamp();
        let target = archive_root.join(format!("{normed}-{stamp}"));
        fs::rename(&dir, target)?;
    }
    // Reset the active-board pointer if it was pointing at this one.
    if get_current_board() == normed {
        let _ = clear_current_board();
    }
    Ok(())
}

pub fn list_boards(include_archived: bool) -> Result<Vec<Board>> {
    let mut out: Vec<Board> = Vec::new();
    // Always include the synthetic default board first.
    out.push(Board {
        slug: DEFAULT_BOARD.to_string(),
        name: "Default".to_string(),
        description: None,
        icon: None,
        created_at: 0,
        is_default: true,
    });
    let root = boards_root();
    if !root.exists() {
        return Ok(out);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == DEFAULT_BOARD {
            continue;
        }
        if name == "_archived" {
            if include_archived {
                for archived in fs::read_dir(&path)? {
                    let archived = archived?;
                    if !archived.path().is_dir() {
                        continue;
                    }
                    let aname = archived.file_name().to_string_lossy().to_string();
                    out.push(Board {
                        slug: format!("_archived/{aname}"),
                        name: aname,
                        description: Some("(archived)".into()),
                        icon: None,
                        created_at: 0,
                        is_default: false,
                    });
                }
            }
            continue;
        }
        let meta = read_board_meta(&name).unwrap_or_default();
        out.push(Board {
            slug: name.clone(),
            name: if meta.name.is_empty() {
                name
            } else {
                meta.name
            },
            description: meta.description,
            icon: meta.icon,
            created_at: meta.created_at,
            is_default: false,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::test_env::with_temp_home;

    #[test]
    fn slug_validation() {
        assert_eq!(
            normalize_board_slug(Some("atm10-server")).as_deref(),
            Some("atm10-server")
        );
        assert_eq!(
            normalize_board_slug(Some("ATM10")).as_deref(),
            Some("atm10")
        );
        assert_eq!(normalize_board_slug(Some("")), None);
        assert_eq!(normalize_board_slug(Some("../bad")), None);
        assert_eq!(normalize_board_slug(Some("-bad")), None);
    }

    #[test]
    fn default_always_exists() {
        with_temp_home(|_| {
            assert!(board_exists(Some(DEFAULT_BOARD)));
            assert!(board_exists(None));
        });
    }

    #[test]
    fn create_and_list_boards() {
        with_temp_home(|_| {
            let b = create_board(
                "atm10-server",
                Some("ATM10 Server"),
                Some("desc"),
                Some("🎮"),
            )
            .unwrap();
            assert_eq!(b.slug, "atm10-server");
            let listed = list_boards(false).unwrap();
            assert!(listed.iter().any(|b| b.slug == "atm10-server"));
            assert!(listed.iter().any(|b| b.is_default));
        });
    }

    #[test]
    fn switch_and_get_current() {
        with_temp_home(|_| {
            create_board("alpha", None, None, None).unwrap();
            set_current_board("alpha").unwrap();
            assert_eq!(get_current_board(), "alpha");
            clear_current_board().unwrap();
            assert_eq!(get_current_board(), DEFAULT_BOARD);
        });
    }
}
