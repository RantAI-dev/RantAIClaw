use std::path::Path;

pub fn assert_path_writable(p: &Path) -> anyhow::Result<()> {
    if p.exists() {
        let test_file = p.join(".rantaiclaw_writetest");
        std::fs::write(&test_file, b"")?;
        std::fs::remove_file(&test_file)?;
        Ok(())
    } else if let Some(parent) = p.parent() {
        let test_file = parent.join(".rantaiclaw_writetest");
        std::fs::write(&test_file, b"")?;
        std::fs::remove_file(&test_file)?;
        Ok(())
    } else {
        anyhow::bail!("path has no parent: {}", p.display())
    }
}

pub fn assert_path_exists(p: &Path) -> anyhow::Result<()> {
    if p.exists() {
        Ok(())
    } else {
        anyhow::bail!("path does not exist: {}", p.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_path_writable_succeeds_for_tempdir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(assert_path_writable(tmp.path()).is_ok());
    }

    #[test]
    fn assert_path_writable_fails_for_nonexistent_parentless_path() {
        assert!(assert_path_writable(std::path::Path::new("/__nonexistent_root__")).is_err());
    }

    #[test]
    fn assert_path_exists_succeeds_for_tempdir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(assert_path_exists(tmp.path()).is_ok());
    }

    #[test]
    fn assert_path_exists_fails_for_nonexistent() {
        assert!(assert_path_exists(std::path::Path::new("/nonexistent_path_12345")).is_err());
    }
}
