//! On-disk, file-locked, multi-claim pairing-code store.
//!
//! Decouples the *minting* process (CLI / chat tool / TUI) from the
//! *validating* process (the running daemon). Both read and write a single
//! JSON file (`pairing_codes.json`) under the profile root, serialized by an
//! `fs2` advisory lock on a sibling `.lock` file so concurrent minters and
//! consumers never corrupt the store.
//!
//! A pairing code is a short Crockford-base32 string rendered grouped as
//! `XXXX-XXXX`. Only its SHA-256 hash is persisted; the plaintext is returned
//! once at mint time and never stored. Each entry is scoped to a single
//! `surface` (channel name or `"gateway"`) so a code minted for one channel can
//! never be claimed on another (identity types differ).
//!
//! Entries are time-windowed (`expires_at`) and may be multi-claim (`max_uses`
//! — `None` = unlimited within the window). Expired entries are pruned on every
//! write. All time-dependent functions take `now` (unix seconds) as a parameter
//! so callers control the clock and tests stay deterministic.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

use crate::security::pairing::constant_time_eq;

/// Name of the JSON store file under the profile root.
const STORE_FILE: &str = "pairing_codes.json";
/// Name of the sibling advisory-lock file under the profile root.
const LOCK_FILE: &str = "pairing_codes.lock";

/// Crockford base32 alphabet (excludes I, L, O, U to avoid ambiguity).
const CHARSET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Number of random base32 characters in a code (~40 bits of entropy).
const CODE_LEN: usize = 8;

/// The on-disk store: a flat list of live code entries.
#[derive(Serialize, Deserialize, Default)]
struct Store {
    codes: Vec<Entry>,
}

/// A single minted pairing code (hash only — plaintext is never persisted).
#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    /// SHA-256 hex of the normalized plaintext code.
    code_hash: String,
    /// Surface this code is scoped to (channel name or `"gateway"`).
    surface: String,
    /// Unix-seconds expiry (inclusive boundary treated as live; see `is_live`).
    expires_at: i64,
    /// Maximum number of successful claims; `None` = unlimited within window.
    max_uses: Option<u32>,
    /// Number of successful claims so far.
    uses: u32,
    /// Whether `/claim` (owner) is permitted with this code.
    grant_owner: bool,
}

impl Entry {
    /// Whether this entry is still claimable at `now` (not expired, not exhausted).
    fn is_live(&self, now: i64) -> bool {
        if now >= self.expires_at {
            return false;
        }
        match self.max_uses {
            Some(max) => self.uses < max,
            None => true,
        }
    }
}

/// Outcome of a successful `try_consume`.
pub struct ConsumeOutcome {
    /// Whether the consumed code allows owner promotion (`/claim`).
    pub grant_owner: bool,
}

/// Path of the JSON store file under `profile_root`.
fn store_path(profile_root: &Path) -> PathBuf {
    profile_root.join(STORE_FILE)
}

/// Path of the advisory-lock file under `profile_root`.
fn lock_path(profile_root: &Path) -> PathBuf {
    profile_root.join(LOCK_FILE)
}

/// Generate a fresh pairing code: 8 Crockford-base32 chars rendered `XXXX-XXXX`.
///
/// Uses `rand::random()` for the raw bytes (OS CSPRNG, same source as the
/// gateway bearer-token generator). `256 % 32 == 0`, so mapping each byte
/// modulo 32 onto the charset is bias-free.
fn generate_code() -> String {
    let bytes: [u8; CODE_LEN] = rand::random();
    let mut out = String::with_capacity(CODE_LEN + 1);
    for (i, b) in bytes.iter().enumerate() {
        if i == CODE_LEN / 2 {
            out.push('-');
        }
        out.push(CHARSET[(*b as usize) % CHARSET.len()] as char);
    }
    out
}

/// Normalize a code for hashing: trim, uppercase, strip group separators.
fn normalize(code: &str) -> String {
    code.trim().to_ascii_uppercase().replace('-', "")
}

/// SHA-256 hex of the normalized code.
fn hash(code: &str) -> String {
    format!("{:x}", Sha256::digest(normalize(code).as_bytes()))
}

/// Load the store from disk; returns an empty store if missing or unparseable.
fn load(profile_root: &Path) -> Store {
    let path = store_path(profile_root);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Store::default(),
    }
}

/// Persist the store to disk with mode `0600` on unix.
fn save(profile_root: &Path, store: &Store) -> Result<()> {
    let path = store_path(profile_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create profile dir {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(store).context("serialize pairing store")?;
    fs::write(&path, &json).with_context(|| format!("write pairing store {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&path)
            .with_context(|| format!("stat pairing store {}", path.display()))?
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms)
            .with_context(|| format!("chmod 0600 pairing store {}", path.display()))?;
    }
    Ok(())
}

/// Acquire the exclusive advisory lock for the store, held for the duration of
/// the returned guard. The lock file is created if absent.
fn lock(profile_root: &Path) -> Result<fs::File> {
    use fs2::FileExt;
    let path = lock_path(profile_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create profile dir {}", parent.display()))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open pairing lock {}", path.display()))?;
    FileExt::lock_exclusive(&file)
        .with_context(|| format!("lock pairing store {}", path.display()))?;
    Ok(file)
}

/// Drop expired/exhausted entries from a loaded store.
fn prune_store(store: &mut Store, now: i64) {
    store.codes.retain(|e| e.is_live(now));
}

/// Generate a code for `surface`, persist its hash, and return the plaintext.
///
/// `ttl_secs` is the validity window from `now`; `max_uses` bounds claims
/// (`None` = unlimited within the window); `grant_owner` permits `/claim`.
/// Expired entries are pruned on write.
pub fn mint(
    profile_root: &Path,
    surface: &str,
    ttl_secs: i64,
    max_uses: Option<u32>,
    grant_owner: bool,
    now: i64,
) -> Result<String> {
    let _guard = lock(profile_root)?;
    let mut store = load(profile_root);
    prune_store(&mut store, now);

    let code = generate_code();
    store.codes.push(Entry {
        code_hash: hash(&code),
        surface: surface.to_string(),
        expires_at: now.saturating_add(ttl_secs),
        max_uses,
        uses: 0,
        grant_owner,
    });
    save(profile_root, &store)?;
    Ok(code)
}

/// Validate and consume a code for `surface`.
///
/// Matches the SHA-256 hash (constant-time) against every live, surface-scoped
/// entry. On a hit, increments `uses`, persists, and returns the outcome.
/// Expired entries are pruned on write. Returns `Ok(None)` on no match.
pub fn try_consume(
    profile_root: &Path,
    surface: &str,
    code: &str,
    now: i64,
) -> Result<Option<ConsumeOutcome>> {
    let _guard = lock(profile_root)?;
    let mut store = load(profile_root);
    prune_store(&mut store, now);

    let target = hash(code);
    let mut outcome = None;
    for entry in &mut store.codes {
        if entry.surface == surface
            && entry.is_live(now)
            && constant_time_eq(&entry.code_hash, &target)
        {
            entry.uses = entry.uses.saturating_add(1);
            outcome = Some(ConsumeOutcome {
                grant_owner: entry.grant_owner,
            });
            break;
        }
    }

    // Re-prune so an entry that just hit its `max_uses` is dropped.
    prune_store(&mut store, now);
    save(profile_root, &store)?;
    Ok(outcome)
}

/// Drop expired/exhausted entries from the store.
pub fn prune(profile_root: &Path, now: i64) -> Result<()> {
    let _guard = lock(profile_root)?;
    let mut store = load(profile_root);
    prune_store(&mut store, now);
    save(profile_root, &store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_code_is_grouped_eight_chars() {
        let code = generate_code();
        // "XXXX-XXXX" = 9 chars including the separator.
        assert_eq!(code.len(), 9);
        assert_eq!(code.as_bytes()[4], b'-');
        let stripped = code.replace('-', "");
        assert_eq!(stripped.len(), CODE_LEN);
        assert!(stripped
            .bytes()
            .all(|b| CHARSET.contains(&b.to_ascii_uppercase())));
    }

    #[test]
    fn normalize_trims_uppercases_strips_dashes() {
        assert_eq!(normalize("  abcd-efgh "), "ABCDEFGH");
        assert_eq!(normalize("ABCDEFGH"), "ABCDEFGH");
    }

    #[test]
    fn mint_then_consume_within_window_multi_use() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let code = mint(p, "telegram", 900, None, true, 1_000).unwrap();

        // First claim ok and owner-capable.
        let r1 = try_consume(p, "telegram", &code, 1_100).unwrap();
        assert!(r1.is_some());
        assert!(r1.unwrap().grant_owner);

        // Second claim still ok (multi-claim window, unlimited uses).
        assert!(try_consume(p, "telegram", &code, 1_200).unwrap().is_some());

        // Wrong surface rejected.
        assert!(try_consume(p, "whatsapp", &code, 1_200).unwrap().is_none());

        // After expiry rejected.
        assert!(try_consume(p, "telegram", &code, 2_000).unwrap().is_none());
    }

    #[test]
    fn consume_is_insensitive_to_case_dashes_and_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let code = mint(p, "telegram", 900, None, false, 1_000).unwrap();
        let lowered = format!("  {}  ", code.to_lowercase());
        let r = try_consume(p, "telegram", &lowered, 1_100).unwrap();
        assert!(r.is_some());
        assert!(!r.unwrap().grant_owner);
    }

    #[test]
    fn wrong_code_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let _code = mint(p, "telegram", 900, None, true, 1_000).unwrap();
        assert!(try_consume(p, "telegram", "ZZZZ-ZZZZ", 1_100)
            .unwrap()
            .is_none());
    }

    #[test]
    fn wrong_surface_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let code = mint(p, "telegram", 900, None, true, 1_000).unwrap();
        assert!(try_consume(p, "slack", &code, 1_100).unwrap().is_none());
    }

    #[test]
    fn expired_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let code = mint(p, "telegram", 100, None, true, 1_000).unwrap();
        // now == expires_at boundary is treated as expired.
        assert!(try_consume(p, "telegram", &code, 1_100).unwrap().is_none());
        assert!(try_consume(p, "telegram", &code, 1_101).unwrap().is_none());
    }

    #[test]
    fn max_uses_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let code = mint(p, "telegram", 900, Some(2), true, 1_000).unwrap();
        assert!(try_consume(p, "telegram", &code, 1_010).unwrap().is_some());
        assert!(try_consume(p, "telegram", &code, 1_020).unwrap().is_some());
        // Third claim exceeds max_uses.
        assert!(try_consume(p, "telegram", &code, 1_030).unwrap().is_none());
    }

    #[test]
    fn prune_drops_expired() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let _short = mint(p, "telegram", 100, None, true, 1_000).unwrap();
        let live = mint(p, "telegram", 10_000, None, true, 1_000).unwrap();

        prune(p, 5_000).unwrap();

        // The expired one is gone; the live one still consumes.
        let store = load(p);
        assert_eq!(store.codes.len(), 1);
        assert!(try_consume(p, "telegram", &live, 5_100).unwrap().is_some());
    }

    #[test]
    fn missing_store_file_yields_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert!(try_consume(p, "telegram", "ABCD-EFGH", 1_000)
            .unwrap()
            .is_none());
    }

    #[test]
    fn distinct_mints_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let owner_code = mint(p, "telegram", 900, None, true, 1_000).unwrap();
        let guest_code = mint(p, "telegram", 900, None, false, 1_000).unwrap();
        assert_ne!(owner_code, guest_code);

        assert!(
            !try_consume(p, "telegram", &guest_code, 1_100)
                .unwrap()
                .unwrap()
                .grant_owner
        );
        assert!(
            try_consume(p, "telegram", &owner_code, 1_100)
                .unwrap()
                .unwrap()
                .grant_owner
        );
    }

    #[cfg(unix)]
    #[test]
    fn store_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let _code = mint(p, "telegram", 900, None, true, 1_000).unwrap();
        let mode = fs::metadata(store_path(p)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
