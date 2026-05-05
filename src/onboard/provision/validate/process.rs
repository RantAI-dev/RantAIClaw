pub fn validate_command_on_path(cmd: &str) -> anyhow::Result<std::path::PathBuf> {
    which::which(cmd).map_err(|e| anyhow::anyhow!("{cmd} not found on PATH: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_command_on_path_finds_sh() {
        assert!(validate_command_on_path("sh").is_ok());
    }

    #[test]
    fn validate_command_on_path_rejects_missing() {
        assert!(validate_command_on_path("__definitely_not_a_command__").is_err());
    }
}
