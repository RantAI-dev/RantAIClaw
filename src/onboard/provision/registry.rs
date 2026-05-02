//! Provisioner factory registry.

use super::traits::TuiProvisioner;

/// Returns `None` if no provisioner matches `name`. Names are kebab-case.
pub fn provisioner_for(name: &str) -> Option<Box<dyn TuiProvisioner>> {
    match name {
        // Filled in by Task 7.
        _ => None,
    }
}

/// Returns all available provisioners as (name, description) pairs.
pub fn available() -> Vec<(&'static str, &'static str)> {
    // Updated in Task 7 when WhatsApp Web is registered.
    vec![]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_returns_none_for_unknown() {
        assert!(provisioner_for("nope").is_none());
    }

    #[test]
    #[ignore = "filled in by task 7"]
    fn registry_lists_at_least_one_name() {
        assert!(!available().is_empty());
    }
}
