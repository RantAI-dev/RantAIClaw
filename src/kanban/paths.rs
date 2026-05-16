use std::path::PathBuf;

use crate::kanban::boards::{get_current_board, normalize_board_slug, DEFAULT_BOARD};

/// Return the umbrella kanban root.
///
/// Resolution order (highest precedence first):
/// 1. `RANTAICLAW_KANBAN_HOME` env var — explicit override (tests, Docker).
/// 2. `RANTAICLAW_HOME` env var — global rantaiclaw root.
/// 3. `~/.rantaiclaw` — install default.
pub fn kanban_home() -> PathBuf {
    if let Ok(p) = std::env::var("RANTAICLAW_KANBAN_HOME") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(shellexpand::tilde(trimmed).into_owned());
        }
    }
    if let Ok(p) = std::env::var("RANTAICLAW_HOME") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(shellexpand::tilde(trimmed).into_owned());
        }
    }
    let home = directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".rantaiclaw")
}

/// `<kanban_home>/kanban/boards` — parent of every named board's directory.
pub fn boards_root() -> PathBuf {
    kanban_home().join("kanban").join("boards")
}

/// `<kanban_home>/kanban/current` — one-line text file with the active slug.
pub fn current_board_path() -> PathBuf {
    kanban_home().join("kanban").join("current")
}

/// Return the directory for `board`. The `default` board's metadata directory
/// is `<root>/kanban/boards/default/`, but its DB stays at `<root>/kanban.db`
/// for back-compat with pre-boards installs.
pub fn board_dir(board: Option<&str>) -> PathBuf {
    let slug = normalize_board_slug(board).unwrap_or_else(|| DEFAULT_BOARD.to_string());
    boards_root().join(slug)
}

/// Resolve the path to the kanban DB for `board`. Order:
/// 1. `RANTAICLAW_KANBAN_DB` env var — pins the path directly.
/// 2. `board` arg (when Some).
/// 3. `RANTAICLAW_KANBAN_BOARD` env / `<root>/kanban/current` / `default`.
///
/// The `default` board's DB stays at `<root>/kanban.db`; other boards live
/// under `<root>/kanban/boards/<slug>/kanban.db`.
pub fn kanban_db_path(board: Option<&str>) -> PathBuf {
    if let Ok(p) = std::env::var("RANTAICLAW_KANBAN_DB") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(shellexpand::tilde(trimmed).into_owned());
        }
    }
    let slug = match board {
        Some(s) => normalize_board_slug(Some(s)).unwrap_or_else(|| DEFAULT_BOARD.to_string()),
        None => get_current_board(),
    };
    if slug == DEFAULT_BOARD {
        kanban_home().join("kanban.db")
    } else {
        boards_root().join(&slug).join("kanban.db")
    }
}

pub fn workspaces_root(board: Option<&str>) -> PathBuf {
    let slug = match board {
        Some(s) => normalize_board_slug(Some(s)).unwrap_or_else(|| DEFAULT_BOARD.to_string()),
        None => get_current_board(),
    };
    if slug == DEFAULT_BOARD {
        kanban_home().join("kanban").join("workspaces")
    } else {
        boards_root().join(&slug).join("workspaces")
    }
}

pub fn logs_root(board: Option<&str>) -> PathBuf {
    let slug = match board {
        Some(s) => normalize_board_slug(Some(s)).unwrap_or_else(|| DEFAULT_BOARD.to_string()),
        None => get_current_board(),
    };
    if slug == DEFAULT_BOARD {
        kanban_home().join("kanban").join("logs")
    } else {
        boards_root().join(&slug).join("logs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kanban::test_env::with_temp_home;

    #[test]
    fn default_board_db_is_at_root() {
        with_temp_home(|home| {
            let p = kanban_db_path(Some("default"));
            assert_eq!(p, home.join("kanban.db"));
        });
    }

    #[test]
    fn named_board_db_under_boards_dir() {
        with_temp_home(|home| {
            let p = kanban_db_path(Some("atm10-server"));
            assert_eq!(
                p,
                home.join("kanban")
                    .join("boards")
                    .join("atm10-server")
                    .join("kanban.db")
            );
        });
    }
}
