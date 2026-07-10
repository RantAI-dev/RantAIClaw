//! Opaque fingerprint of the raw config-file bytes, so a client (e.g.
//! `ui start`) can tell whether a running gateway is on the current on-disk
//! config without reading any config contents.

use std::path::Path;

use sha2::{Digest, Sha256};

/// 16-hex-char prefix of `sha256(raw file bytes)`. Returns `"none"` when the
/// file cannot be read — a value that never collides with a real hash, so a
/// comparison against it always signals drift.
pub fn fingerprint_file(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let digest = Sha256::digest(bytes);
            hex::encode(&digest[..8])
        }
        Err(_) => "none".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn fingerprint_changes_with_content_and_is_stable() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "port = 3000").unwrap();
        let a = fingerprint_file(f.path());
        assert_eq!(
            a,
            fingerprint_file(f.path()),
            "same bytes → same fingerprint"
        );
        assert_eq!(a.len(), 16, "16 hex chars");

        std::fs::write(f.path(), b"port = 9393\n").unwrap();
        assert_ne!(
            a,
            fingerprint_file(f.path()),
            "changed bytes → changed fingerprint"
        );
    }

    #[test]
    fn missing_file_is_none() {
        assert_eq!(fingerprint_file(Path::new("/no/such/file")), "none");
    }
}
