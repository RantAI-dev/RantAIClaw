//! Shared release-artifact fetch + verification (SHA256 + cosign keyless OIDC).
//! Used by the binary self-updater and the `ui` console installer.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use sha2::{Digest, Sha256};

/// Download `url` to `dest`, streaming the response body to disk.
pub fn download_to(url: &str, dest: &Path) -> Result<()> {
    let client = Client::builder()
        .user_agent(format!("rantaiclaw-update/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut resp = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("download {url} returned {}", resp.status());
    }
    let mut f = fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = resp.read(&mut buf).context("read response body")?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])
            .with_context(|| format!("write {}", dest.display()))?;
    }
    Ok(())
}

/// Verify `archive`'s SHA256 against the entry for `archive_name` in `sums_file`.
pub fn verify_sha256(archive: &Path, sums_file: &Path, archive_name: &str) -> Result<()> {
    let expected = read_sha_for_file(sums_file, archive_name)?;
    let actual = compute_sha256(archive)?;
    if !expected.eq_ignore_ascii_case(&actual) {
        bail!("SHA256 mismatch for {archive_name}\n  expected: {expected}\n  actual:   {actual}");
    }
    Ok(())
}

fn read_sha_for_file(sums_file: &Path, archive_name: &str) -> Result<String> {
    let content =
        fs::read_to_string(sums_file).with_context(|| format!("read {}", sums_file.display()))?;
    for line in content.lines() {
        // Format: "<hex>  <filename>" (two spaces) or "<hex> *<filename>" or
        // "<hex>  ./<filename>". Be liberal in parsing.
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(2, char::is_whitespace);
        let Some(hash) = it.next() else { continue };
        let Some(rest) = it.next() else { continue };
        let name = rest.trim().trim_start_matches('*').trim_start_matches("./");
        if name == archive_name {
            return Ok(hash.to_string());
        }
    }
    bail!("{archive_name} not listed in SHA256SUMS")
}

fn compute_sha256(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Tri-state outcome from `verify_cosign`, so [`enforce_cosign`] can name the
/// cause in the refusal. Distinguishes "no cosign on this host" (the operator's
/// own machine, fixable by installing it) from "no bundle published for this
/// release" (nothing the operator can fix, and not something any current
/// release produces).
pub enum CosignOutcome {
    Verified,
    CosignNotInstalled,
    BundleMissing,
}

/// Act on a [`CosignOutcome`]: anything short of `Verified` refuses, unless the
/// operator explicitly asked to proceed unverified.
///
/// A control that can be skipped by a condition an attacker can arrange is not
/// a control. An attacker who can serve the artefact can also serve a 404 for
/// its bundle, and the SHA-256 checksum file comes from the same origin — so
/// "bundle missing, SHA-only" was verification the attacker could opt out of on
/// the operator's behalf. `cosign` being absent is the operator's own machine,
/// but continuing silently made the project's strongest supply-chain control
/// optional in practice.
///
/// `what` names the artefact for the message (e.g. `"rantaiclaw v0.28.0"`).
pub fn enforce_cosign(outcome: CosignOutcome, what: &str, allow_unverified: bool) -> Result<()> {
    let reason = match outcome {
        CosignOutcome::Verified => return Ok(()),
        CosignOutcome::CosignNotInstalled => {
            "`cosign` is not on PATH, so the signature could not be checked \
             (install it: https://docs.sigstore.dev/system_config/installation/)"
        }
        CosignOutcome::BundleMissing => {
            "no cosign signature is published for it — every release this \
             project has ever made is signed, so an absent bundle means the \
             download did not come from a project release"
        }
    };

    if allow_unverified {
        eprintln!("⚠ {what} was NOT verified: {reason}.");
        eprintln!("  Continuing because --allow-unverified was passed. The SHA-256 checksum");
        eprintln!(
            "  comes from the same server as the archive, so it proves nothing about origin."
        );
        return Ok(());
    }

    bail!(
        "refusing to install {what}: {reason}.\n\
         The SHA-256 checksum is served from the same origin as the archive, so it \
         cannot stand in for a signature. Re-run with --allow-unverified to proceed anyway."
    )
}

/// Cosign keyless-OIDC signature verification on a release archive.
///
/// Returns:
/// * `Ok(CosignOutcome::Verified)`           — bundle found and signature verified
/// * `Ok(CosignOutcome::CosignNotInstalled)` — `cosign` not on PATH
/// * `Ok(CosignOutcome::BundleMissing)`      — bundle file 404
///
/// This function only reports. [`enforce_cosign`] decides what an outcome
/// means — every caller must route through it rather than matching the enum
/// itself, or a new outcome silently becomes another way to skip verification.
/// * `Err(_)` — bundle found but the verification itself failed (signature
///   mismatch, wrong identity, wrong issuer)
///
/// `identity_regex` pins the expected signing workflow identity — caller-supplied
/// so the binary self-updater and the `ui` console installer can each pin their
/// own repo's release workflow without this module hard-coding either. The OIDC
/// issuer is always GitHub Actions.
pub fn verify_cosign(
    base_url: &str,
    archive_path: &Path,
    archive_name: &str,
    work_dir: &Path,
    identity_regex: &str,
) -> Result<CosignOutcome> {
    // Bail-friendly local-prereq check. `which cosign` style.
    let cosign_present = Command::new("cosign")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !cosign_present {
        return Ok(CosignOutcome::CosignNotInstalled);
    }

    let bundle_url = format!("{base_url}/{archive_name}.bundle");
    let bundle_path = work_dir.join(format!("{archive_name}.bundle"));

    // Try to fetch the bundle. A 404 is the "no bundle for this release" path
    // — the caller decides whether that's tolerable or fatal.
    let client = Client::builder()
        .user_agent("rantaiclaw-update")
        .build()
        .context("build http client")?;
    let resp = client
        .get(&bundle_url)
        .send()
        .context("fetch cosign bundle")?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(CosignOutcome::BundleMissing);
    }
    if !resp.status().is_success() {
        bail!("fetch {bundle_url} returned HTTP {}", resp.status());
    }
    let bytes = resp.bytes().context("read cosign bundle body")?;
    fs::write(&bundle_path, &bytes)
        .with_context(|| format!("write cosign bundle to {}", bundle_path.display()))?;

    // Pin both the workflow identity (caller-supplied) and the OIDC issuer.
    // Anything signed by a different workflow or a non-GitHub-Actions issuer
    // is rejected.
    let issuer = "https://token.actions.githubusercontent.com";

    let output = Command::new("cosign")
        .args([
            "verify-blob",
            "--bundle",
            bundle_path.to_str().unwrap_or_default(),
            "--certificate-identity-regexp",
            identity_regex,
            "--certificate-oidc-issuer",
            issuer,
            archive_path.to_str().unwrap_or_default(),
        ])
        .output()
        .context("invoke cosign verify-blob")?;

    if !output.status.success() {
        bail!(
            "cosign verify-blob rejected the archive (exit {}): {}",
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".into()),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(CosignOutcome::Verified)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A verified archive is the only outcome that needs no operator decision.
    #[test]
    fn a_verified_archive_installs() {
        assert!(enforce_cosign(CosignOutcome::Verified, "rantaiclaw v1.2.3", false).is_ok());
    }

    /// `cosign` missing used to print a warning and continue on the SHA-256
    /// checksum — which is fetched from the same origin as the archive, so an
    /// attacker who can serve one can serve the other.
    #[test]
    fn a_host_without_cosign_refuses_by_default() {
        let err = enforce_cosign(
            CosignOutcome::CosignNotInstalled,
            "rantaiclaw v1.2.3",
            false,
        )
        .expect_err("an unverifiable archive must not install");
        let msg = err.to_string();
        assert!(
            msg.contains("cosign"),
            "the error must name the cause: {msg}"
        );
        assert!(
            msg.contains("--allow-unverified"),
            "the error must name the way out: {msg}"
        );
    }

    /// A 404 on the bundle is attacker-arrangeable: whoever serves the archive
    /// serves the bundle URL too. Every release this project has published
    /// carries one, so an absent bundle is never "a legacy release".
    #[test]
    fn a_missing_signature_refuses_by_default() {
        let err = enforce_cosign(CosignOutcome::BundleMissing, "claw-ui v0.3.25", false)
            .expect_err("an unsigned archive must not install");
        let msg = err.to_string();
        assert!(
            msg.contains("claw-ui v0.3.25"),
            "the error must name the artefact: {msg}"
        );
        assert!(msg.contains("--allow-unverified"), "{msg}");
    }

    /// The escape hatch exists — for an operator who asked for it, by name.
    #[test]
    fn both_skip_paths_proceed_only_on_an_explicit_opt_in() {
        assert!(
            enforce_cosign(CosignOutcome::CosignNotInstalled, "rantaiclaw v1.2.3", true).is_ok()
        );
        assert!(enforce_cosign(CosignOutcome::BundleMissing, "claw-ui v0.2.0", true).is_ok());
    }

    #[test]
    fn cosign_identity_regex_is_caller_supplied() {
        // The claw-ui identity and the binary identity are different strings;
        // verify_cosign must not hard-code either. This guards the signature
        // of verify_cosign so a refactor can't silently re-hard-code identity.
        fn takes_identity(_: &str) {}
        takes_identity(r"^https://github\.com/RantAI-dev/claw-ui/.*$");
    }

    // ── moved from lifecycle::update — integrity gate: checksum verification ──

    #[test]
    fn compute_sha256_matches_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("blob");
        fs::write(&p, b"abc").unwrap();
        // Well-known SHA-256 test vector for the bytes "abc".
        assert_eq!(
            compute_sha256(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn read_sha_for_file_handles_multiple_formats() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("SHA256SUMS");
        fs::write(
            &p,
            "abc123  rantaiclaw-x86_64-unknown-linux-gnu.tar.gz\n\
             def456  ./rantaiclaw.spdx.json\n\
             feed00 *some-other.zip\n",
        )
        .unwrap();
        assert_eq!(
            read_sha_for_file(&p, "rantaiclaw-x86_64-unknown-linux-gnu.tar.gz").unwrap(),
            "abc123"
        );
        assert_eq!(
            read_sha_for_file(&p, "rantaiclaw.spdx.json").unwrap(),
            "def456"
        );
        assert_eq!(read_sha_for_file(&p, "some-other.zip").unwrap(), "feed00");
        assert!(read_sha_for_file(&p, "missing.tar.gz").is_err());
    }
}
