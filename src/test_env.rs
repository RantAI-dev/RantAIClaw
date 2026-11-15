//! Shared, process-wide serialization for tests that mutate config-resolution
//! environment variables.
//!
//! `Config::load_or_init` (and the profile/store resolution beneath it) reads
//! **process-global** env vars — `HOME`, `RANTAICLAW_CONFIG_DIR`,
//! `RANTAICLAW_WORKSPACE`, `RANTAICLAW_PROFILE`. `cargo test --lib` runs every
//! unit test in one process across many threads, so a per-module lock does
//! **not** serialize a test in `channels::slack` against one in
//! `channels::mattermost`: they hold different mutexes and clobber each other's
//! env var mid-test, which surfaced as flaky `unwrap()`-on-`None` panics.
//!
//! Every test that sets one of those vars must acquire THIS single lock:
//! - async tests (`#[tokio::test]`): `test_env::ENV_LOCK.lock().await`
//! - sync tests (`#[test]`, no runtime): `test_env::ENV_LOCK.blocking_lock()`
//!
//! It is a `tokio::sync::Mutex` (not `std::sync::Mutex`) so the async tests can
//! hold the guard across `.await` points; `blocking_lock()` covers the sync
//! callers, which run outside any runtime.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

pub(crate) static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Point `HOME` at a temp dir and put the previous value back on drop, so a
/// panicking test does not leak the override into the next one.
///
/// `HOME` is the lever that moves per-profile state: the profile root is
/// `home_dir()/.rantaiclaw/profiles/<name>` (`profile/paths.rs`), so
/// `RANTAICLAW_CONFIG_DIR` does **not** move `sessions.db` — pinning that one
/// instead leaves the test writing into the operator's real session history.
///
/// Only meaningful while `ENV_LOCK` is held.
/// Unset every credential variable a test must not inherit from the developer's
/// own shell, restoring them all on drop.
///
/// `has_usable_credential` reads the environment, so any test asserting "no
/// credential anywhere" passes vacuously on a machine with `OPENROUTER_API_KEY`
/// exported — and asserts the opposite of what it means on a machine without
/// it. Both doctor and providers need this, so it lives here rather than as a
/// third copy.
///
/// Only meaningful while [`ENV_LOCK`] is held.
pub(crate) struct CredentialEnvScrub(#[allow(dead_code)] Vec<EnvGuard>);

impl CredentialEnvScrub {
    pub(crate) fn new() -> Self {
        Self(
            [
                "RANTAICLAW_API_KEY",
                "API_KEY",
                "OPENAI_API_KEY",
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_OAUTH_TOKEN",
                "OPENROUTER_API_KEY",
                "GROQ_API_KEY",
                "OLLAMA_API_KEY",
                "GEMINI_API_KEY",
                "GOOGLE_API_KEY",
                "DASHSCOPE_API_KEY",
                "MINIMAX_API_KEY",
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
                "QWEN_OAUTH_TOKEN",
                "QWEN_OAUTH_REFRESH_TOKEN",
                "QWEN_OAUTH_RESOURCE_URL",
                "MINIMAX_OAUTH_TOKEN",
                "MINIMAX_OAUTH_REFRESH_TOKEN",
                "XDG_CONFIG_HOME",
            ]
            .into_iter()
            .map(EnvGuard::unset)
            .collect(),
        )
    }
}

pub(crate) struct HomeGuard(Option<OsString>);

impl HomeGuard {
    pub(crate) fn set(path: &Path) -> Self {
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", path);
        Self(prev)
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(prev) => std::env::set_var("HOME", prev),
            None => std::env::remove_var("HOME"),
        }
    }
}

/// Set (or clear) an arbitrary env var and put the previous value back on drop,
/// so a test that panics between the set and its trailing `remove_var` does not
/// leak the override into the next test sharing `ENV_LOCK`. The generic sibling
/// of [`HomeGuard`] — for `RANTAICLAW_API_KEY`, `RANTAICLAW_PROVIDER`,
/// `RANTAICLAW_CONFIG_DIR`, `PORT`, etc.
///
/// Bind it to a NAMED local (`let _guard = EnvGuard::set(...)`), never `let _ =`
/// — the latter drops immediately and restores before the test body runs.
///
/// Only meaningful while `ENV_LOCK` is held.
#[must_use = "the override is reverted the moment this guard is dropped; bind it to a named local"]
pub(crate) struct EnvGuard {
    key: &'static str,
    prev: Option<OsString>,
}

impl EnvGuard {
    /// Set `key` to `value` for the guard's lifetime.
    pub(crate) fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prev }
    }

    /// Ensure `key` is UNSET for the guard's lifetime (restoring any prior value
    /// on drop) — the `remove_var("X"); set_var("Y", …)` precedence pattern.
    pub(crate) fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        Self { key, prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(prev) => std::env::set_var(self.key, prev),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Acquire [`ENV_LOCK`] and point `RANTAICLAW_AUDIT_DIR_OVERRIDE` at a fresh
/// per-test temp directory, so any `record_tool_call` triggered during the
/// test lands there instead of in the operator's real `audit.log`. Returns
/// the two guards so the test body binds them as `let (_env, _audit) = …`.
///
/// The directory is a [`tempfile::TempDir`] held inside the returned
/// [`EnvAuditRedirect`], so it is removed from disk the moment the test drops
/// the guard, rather than left behind under the system temp directory.
#[must_use = "both guards revert the moment they drop; bind them to named locals"]
pub(crate) async fn redirect_audit_temp() -> (EnvAuditRedirect, EnvGuard) {
    let env = ENV_LOCK.lock().await;
    let dir = tempfile::Builder::new()
        .prefix("rantaiclaw-audit-redirect-")
        .tempdir()
        .expect("audit redirect tempdir");
    let audit = EnvGuard::set("RANTAICLAW_AUDIT_DIR_OVERRIDE", dir.path());
    (
        EnvAuditRedirect {
            _env: env,
            _dir: dir,
        },
        audit,
    )
}

/// Marker guard held for as long as the audit redirect must stay in effect.
/// Carries the `ENV_LOCK` guard and the temp directory
/// `RANTAICLAW_AUDIT_DIR_OVERRIDE` points at, so the helper can return a
/// tuple `(env_guard, audit_guard)` without leaking the lock type, and the
/// directory is deleted when the test drops the guard.
#[must_use = "the ENV_LOCK is released the moment this drops; bind it to a named local"]
pub(crate) struct EnvAuditRedirect {
    _env: tokio::sync::MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
}

/// A temp directory holding fake binaries named `systemctl`, `launchctl`,
/// `rc-service`, `rc-update`, and `schtasks`. Prepending the directory to
/// `PATH` lets a test prove that production code does not spawn one of the
/// five service-manager programs: if the guard is in place, the fake is never
/// reached; if the guard is removed (mutation check), the fake answers the
/// query and writes a marker on restart so the failing test can be diagnosed.
///
/// The marker file lives next to the binary and is unique to this guard. The
/// `ENV_LOCK` is NOT acquired here — every caller must hold it for the lifetime
/// of the returned guard.
#[must_use = "PATH is reverted the moment this drops; bind it to a named local"]
pub(crate) struct FakeServiceManager {
    _env: EnvGuard,
    pub(crate) dir: tempfile::TempDir,
}

impl FakeServiceManager {
    pub(crate) fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("rantaiclaw-fake-svcmgr-")
            .tempdir()
            .expect("fake service-manager dir");
        for name in [
            "systemctl",
            "launchctl",
            "rc-service",
            "rc-update",
            "schtasks",
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, FAKE_SERVICE_MANAGER_SCRIPT)
                .unwrap_or_else(|e| panic!("write fake {name}: {e}"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .unwrap_or_else(|e| panic!("chmod fake {name}: {e}"));
            }
        }
        let prev = std::env::var_os("PATH");
        let bin = dir.path().to_string_lossy();
        let new_path = match prev.as_ref().map(|p| p.to_string_lossy().into_owned()) {
            Some(p) => format!("{bin}:{p}"),
            None => bin.into_owned(),
        };
        let env = EnvGuard::set("PATH", &new_path);
        Self { _env: env, dir }
    }

    /// Path of the marker file the `systemctl` fake writes on restart.
    pub(crate) fn restart_marker(&self) -> std::path::PathBuf {
        self.dir.path().join("systemctl.restarted.marker")
    }

    /// Path of the marker file the `rc-service` fake writes on restart.
    pub(crate) fn rc_service_restart_marker(&self) -> std::path::PathBuf {
        self.dir.path().join("rc-service.restarted.marker")
    }
}

// ---------------------------------------------------------------------------
// The guard against the next unguarded writer.
//
// A test that calls `std::env::set_var` / `std::env::remove_var` itself is
// the defect this module exists to prevent: the environment is process-wide,
// `cargo test --lib` runs every unit test in one process across many threads,
// and a write that does not hold `ENV_LOCK` races every other test that reads
// the same variable. The check below reads the sources and names the file and
// the line of every such write, so the next writer is caught here instead of
// as a flaky failure in somebody else's module.
//
// How it tells test code from production code:
// - an item under `#[cfg(test)]`: the whole item, braces matched;
// - a `#[test]` or `#[tokio::test]` function: it is compiled only under
//   `--test`;
// - a file a `#[cfg(test)] mod X;` declaration pulls in.
//
// What it does not flag:
// - `#[cfg(any(test, debug_assertions))]`: that block also compiles in a
//   debug build, so it is not test-only code;
// - `src/test_env.rs`: the guards above write the environment on purpose;
// - `src/main.rs`: the binary is a crate of its own, cannot take the
//   library's lock, and carries a copy of its own.
// - a write inside a `macro_rules!` body: the scanner reads the source
//   text, not the expansion;
// - a `#[cfg(test)] mod X;` declared inside a nested inline module: the
//   scan cannot resolve that path, so it fails the check loudly instead
//   of skipping the file.

/// Every bare environment write that sits in test-only code under `src/`.
///
/// Each finding is the file's path relative to `src/` and the line of the
/// write, so the failure names where to look.
fn bare_env_writes_in_test_code(src_root: &Path) -> Vec<(String, usize)> {
    let files = rust_source_files(src_root);
    let sources: Vec<MaskedSource> = files.iter().map(|path| MaskedSource::read(path)).collect();

    // A file a `#[cfg(test)] mod X;` declaration pulls in is test-only from
    // top to bottom, so those files are collected before the scan.
    let mut test_only_files: Vec<PathBuf> = Vec::new();
    for source in &sources {
        for module in external_test_modules(&source.masked, &source.raw) {
            let target = resolve_test_module(&source.path, &module).unwrap_or_else(|| {
                panic!(
                    "cannot resolve the test module `{}` declared in {}; the scan cannot \
                     tell which file it is, so it cannot check that file — declare it \
                     with a `#[path = \"...\"]` the scan can read",
                    module.describe(),
                    source.path.display()
                )
            });
            test_only_files.push(target);
        }
    }

    let mut findings = Vec::new();
    for source in &sources {
        if is_excluded_from_the_scan(src_root, &source.path) {
            continue;
        }
        let whole_file = test_only_files
            .iter()
            .any(|path| same_path(path, &source.path));
        let ranges = test_only_ranges(&source.masked);
        for offset in env_write_offsets(&source.masked) {
            let in_region = ranges
                .iter()
                .any(|(start, end)| *start <= offset && offset <= *end);
            if whole_file || in_region {
                findings.push((
                    relative_to(src_root, &source.path),
                    line_of(&source.masked, offset),
                ));
            }
        }
    }
    findings
}

/// Whether a file is out of the scan's reach.
///
/// - `src/test_env.rs` holds the guards the scan sends every other file to,
///   so it writes the environment on purpose.
/// - `src/main.rs` is the binary, which is a crate of its own: it cannot
///   take the library's lock, and it carries a copy of its own.
fn is_excluded_from_the_scan(src_root: &Path, path: &Path) -> bool {
    matches!(
        relative_to(src_root, path).as_str(),
        "test_env.rs" | "main.rs"
    )
}

/// No test writes the process environment on its own.
///
/// Every write in test-only code has to go through [`EnvGuard`] or
/// [`HomeGuard`], which restore the previous value on drop, and has to hold
/// [`ENV_LOCK`] while it does so. A bare `std::env::set_var` in a test is
/// the defect this check exists for: it leaks into every other test that
/// shares the process, and it restores nothing when the test panics.
#[test]
fn no_test_code_writes_the_environment_without_the_crate_lock() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let findings = bare_env_writes_in_test_code(&src_root);
    assert!(
        findings.is_empty(),
        "these writes change the process environment in test-only code without \
         `crate::test_env::ENV_LOCK`; use `EnvGuard` / `HomeGuard` instead: {}",
        findings
            .iter()
            .map(|(file, line)| format!("{file}:{line}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// A source file under `src/`, with the bytes the scan reads.
struct MaskedSource {
    /// The path of the file, as the walk found it.
    path: PathBuf,
    /// The file as it is on disk.
    raw: Vec<u8>,
    /// `raw` with every comment and every literal's contents blanked to
    /// spaces, so a brace count and a search for `env::set_var` cannot be
    /// fooled by text that is not code.
    masked: Vec<u8>,
}

impl MaskedSource {
    /// Read one file and mask it.
    fn read(path: &Path) -> Self {
        let raw = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let masked = mask_comments_and_literals(&raw);
        Self {
            path: path.to_path_buf(),
            raw,
            masked,
        }
    }
}

/// A module a `#[cfg(test)]` declaration pulls in from another file.
enum TestModule {
    /// A `#[path = "..."]` attribute names the file.
    Path(String),
    /// A plain module name: `X.rs` or `X/mod.rs` next to the declaration.
    Name(String),
}

impl TestModule {
    /// The module as it reads in a failure message.
    fn describe(&self) -> String {
        match self {
            Self::Path(path) => path.clone(),
            Self::Name(name) => name.clone(),
        }
    }
}

/// Every `.rs` file under `src/`, sorted, so a finding is reported in a
/// stable order.
fn rust_source_files(src_root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![src_root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// The file a `#[cfg(test)]` module declaration pulls in, if it resolves.
fn resolve_test_module(declaring: &Path, module: &TestModule) -> Option<PathBuf> {
    let dir = declaring.parent()?;
    let candidate = match module {
        TestModule::Path(path) => dir.join(path),
        TestModule::Name(name) => {
            let flat = dir.join(format!("{name}.rs"));
            if flat.exists() {
                flat
            } else {
                dir.join(name).join("mod.rs")
            }
        }
    };
    candidate.exists().then_some(candidate)
}

/// Whether two paths name the same file.
fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// A path relative to `src/`, with `/` separators, for the failure message.
fn relative_to(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The 1-based line of a byte offset in a masked source.
fn line_of(code: &[u8], offset: usize) -> usize {
    code[..offset].iter().filter(|byte| **byte == b'\n').count() + 1
}

/// The byte offsets of every `env::set_var` / `env::remove_var` call in a
/// masked source.
///
/// The `env::` prefix is matched without the `std::` in front of it, so the
/// call is found whether the file wrote `std::env::set_var` or imported
/// `std::env` and wrote `env::set_var`. A word boundary on both sides keeps
/// `foo_env::set_var` out.
fn env_write_offsets(code: &[u8]) -> Vec<usize> {
    const NEEDLES: [&[u8]; 2] = [b"env::set_var", b"env::remove_var"];
    let mut offsets = Vec::new();
    for needle in NEEDLES {
        let mut index = 0;
        while index + needle.len() <= code.len() {
            if &code[index..index + needle.len()] == needle {
                let before_ok = index == 0 || !is_ident_byte(code[index - 1]);
                let after = index + needle.len();
                let after_ok = after >= code.len() || !is_ident_byte(code[after]);
                if before_ok && after_ok {
                    offsets.push(index);
                }
                index = after;
            } else {
                index += 1;
            }
        }
    }
    offsets.sort_unstable();
    offsets.dedup();
    offsets
}

/// Whether a byte can be part of an identifier.
fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The byte ranges of the test-only items in a masked source: an item under
/// `#[cfg(test)]`, and the body of a `#[test]` or `#[tokio::test]`
/// function.
///
/// `#[cfg(any(test, debug_assertions))]` is not test-only — the block also
/// compiles in a debug build — so only a `cfg` that *requires* `test` counts.
fn test_only_ranges(code: &[u8]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for (start, end) in attribute_groups(code) {
        let attributes = attributes_in(&code[start..end]);
        let cfg_test = attributes.iter().any(|attr| is_test_only_cfg(attr));
        let test_fn = attributes.iter().any(|attr| is_test_function_attr(attr));
        if !cfg_test && !test_fn {
            continue;
        }
        let item_start = skip_whitespace(code, end);
        let (tokens, position, terminator) = item_head(code, item_start);
        if terminator != Some(b'{') {
            continue;
        }
        let is_block = tokens.is_empty();
        let is_item = tokens
            .iter()
            .any(|token| matches!(*token, "mod" | "fn" | "impl" | "trait" | "enum"));
        if is_block || is_item {
            ranges.push((position, find_matching(code, position)));
        }
    }
    ranges
}

/// The modules a `#[cfg(test)]` declaration pulls in from another file.
fn external_test_modules(masked: &[u8], raw: &[u8]) -> Vec<TestModule> {
    let mut modules = Vec::new();
    for (start, end) in attribute_groups(masked) {
        let attributes = attributes_in(&masked[start..end]);
        if !attributes.iter().any(|attr| is_test_only_cfg(attr)) {
            continue;
        }
        let item_start = skip_whitespace(masked, end);
        let (tokens, _position, terminator) = item_head(masked, item_start);
        if terminator != Some(b';') {
            continue;
        }
        let Some(index) = tokens.iter().position(|token| *token == "mod") else {
            continue;
        };
        let Some(name) = tokens.get(index + 1) else {
            continue;
        };
        match path_attribute(&raw[start..end]) {
            Some(path) => modules.push(TestModule::Path(path)),
            None => modules.push(TestModule::Name((*name).to_string())),
        }
    }
    modules
}

/// The file a `#[path = "..."]` attribute names, if the group carries one.
fn path_attribute(raw_group: &[u8]) -> Option<String> {
    let mut index = 0;
    while index < raw_group.len() {
        if raw_group[index..].starts_with(b"#[") {
            let mut depth = 1_usize;
            let mut end = index + 2;
            while end < raw_group.len() && depth > 0 {
                if raw_group[end] == b'[' {
                    depth += 1;
                } else if raw_group[end] == b']' {
                    depth -= 1;
                }
                end += 1;
            }
            if let Some(value) = string_attribute_value(&raw_group[index..end], b"path") {
                return Some(value);
            }
            index = end;
        } else {
            index += 1;
        }
    }
    None
}

/// The value of a `#[name = "value"]` attribute, when it has that shape.
fn string_attribute_value(attribute: &[u8], name: &[u8]) -> Option<String> {
    let inner = strip_brackets(attribute)?;
    let inner = trim_bytes(inner);
    let rest = inner.strip_prefix(name)?;
    let rest = trim_bytes(rest);
    let rest = trim_bytes(rest.strip_prefix(b"=")?);
    let rest = rest.strip_prefix(b"\"")?;
    let end = rest.iter().position(|byte| *byte == b'"')?;
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// The bytes between an attribute's `#[` and `]`.
fn strip_brackets(attribute: &[u8]) -> Option<&[u8]> {
    let inner = attribute.strip_prefix(b"#[")?;
    inner.strip_suffix(b"]")
}

/// ASCII whitespace at both ends of a byte slice.
fn trim_bytes(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

/// Whether a `cfg` attribute requires `test`.
///
/// `cfg(test)` does, and so does `cfg(all(test, unix))`. `cfg(any(test, ...))`
/// does not: the other branch can enable the item in a build that is not a
/// test build, so the item is not test-only code.
fn is_test_only_cfg(attribute: &[u8]) -> bool {
    let Some(inner) = strip_brackets(attribute) else {
        return false;
    };
    let Some(rest) = inner.strip_prefix(b"cfg(") else {
        return false;
    };
    let rest = trim_bytes(rest.strip_suffix(b")").unwrap_or(rest));
    if rest == b"test" {
        return true;
    }
    let Some(args) = rest.strip_prefix(b"all(") else {
        return false;
    };
    let args = args.strip_suffix(b")").unwrap_or(args);
    split_args(args)
        .iter()
        .any(|arg| trim_bytes(arg) == b"test")
}

/// Whether an attribute marks a test function: `#[test]` or
/// `#[tokio::test]`, with or without arguments.
fn is_test_function_attr(attribute: &[u8]) -> bool {
    let Some(inner) = strip_brackets(attribute) else {
        return false;
    };
    let inner = trim_bytes(inner);
    if inner == b"test" {
        return true;
    }
    let Some(rest) = inner.strip_prefix(b"tokio::test") else {
        return false;
    };
    rest.is_empty() || (rest.starts_with(b"(") && rest.ends_with(b")"))
}

/// The comma-separated arguments of a `cfg` predicate.
fn split_args(args: &[u8]) -> Vec<&[u8]> {
    let mut parts = Vec::new();
    let mut depth = 0_usize;
    let mut start = 0_usize;
    for (index, byte) in args.iter().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                parts.push(&args[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < args.len() {
        parts.push(&args[start..]);
    }
    parts
}

/// The byte ranges of every run of attributes in a masked source.
///
/// A run is the attributes that sit on one item: they follow each other
/// with nothing but whitespace between them. The range covers the
/// attributes only; the item that follows is read separately.
fn attribute_groups(code: &[u8]) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut index = 0;
    while index < code.len() {
        if code[index..].starts_with(b"#[") {
            let start = index;
            let mut end = attribute_end(code, index);
            loop {
                let next = skip_whitespace(code, end);
                if next < code.len() && code[next..].starts_with(b"#[") {
                    end = attribute_end(code, next);
                } else {
                    break;
                }
            }
            groups.push((start, end));
            index = end;
        } else {
            index += 1;
        }
    }
    groups
}

/// The offset just past the `]` that closes the attribute at `start`.
fn attribute_end(code: &[u8], start: usize) -> usize {
    let mut depth = 1_usize;
    let mut index = start + 2;
    while index < code.len() && depth > 0 {
        if code[index] == b'[' {
            depth += 1;
        } else if code[index] == b']' {
            depth -= 1;
        }
        index += 1;
    }
    index
}

/// The individual attributes inside one run of attributes.
fn attributes_in(group: &[u8]) -> Vec<&[u8]> {
    let mut attributes = Vec::new();
    let mut index = 0;
    while index < group.len() {
        if group[index..].starts_with(b"#[") {
            let end = attribute_end(group, index);
            attributes.push(&group[index..end]);
            index = end;
        } else {
            index += 1;
        }
    }
    attributes
}

/// The first byte at or after `from` that is not whitespace.
fn skip_whitespace(code: &[u8], from: usize) -> usize {
    let mut index = from;
    while index < code.len() && code[index].is_ascii_whitespace() {
        index += 1;
    }
    index
}

/// The tokens of the item that starts at `from`, the offset of its `{` or
/// `;`, and which of the two it is.
///
/// The tokens are the words of the item's head: `pub`, `crate`, `mod`,
/// `tests`, and so on. A head that ends in `;` is an item without a body
/// (a module declaration, a `use`), and one that ends in `{` has a body.
fn item_head(code: &[u8], from: usize) -> (Vec<&str>, usize, Option<u8>) {
    let mut tokens: Vec<&str> = Vec::new();
    let mut index = from;
    while index < code.len() {
        let byte = code[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if byte == b'#' {
            if code[index..].starts_with(b"#[") {
                index = attribute_end(code, index);
                continue;
            }
            break;
        }
        if byte == b'{' || byte == b';' {
            return (tokens, index, Some(byte));
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = index;
            while index < code.len() && is_ident_byte(code[index]) {
                index += 1;
            }
            tokens.push(std::str::from_utf8(&code[start..index]).unwrap_or_default());
            continue;
        }
        index += 1;
    }
    (tokens, index, None)
}

/// The offset of the brace that closes the one at `open`.
///
/// The source is masked, so a brace inside a string or a comment is a space
/// and cannot be counted.
fn find_matching(code: &[u8], open: usize) -> usize {
    let mut depth = 0_usize;
    for (index, byte) in code.iter().enumerate().skip(open) {
        match byte {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    code.len().saturating_sub(1)
}

/// The source with every comment and every literal's contents blanked to
/// spaces, so the scan reads code only.
///
/// Newlines stay, so a byte offset still names the line it was on. Nested
/// block comments, raw strings and a quote inside a character literal are
/// handled; a lifetime has no closing quote and is left alone.
fn mask_comments_and_literals(src: &[u8]) -> Vec<u8> {
    let mut code = src.to_vec();
    let mut index = 0;
    while index < src.len() {
        if src[index..].starts_with(b"//") {
            let end = src[index..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(src.len(), |offset| index + offset);
            blank(&mut code, index..end);
            index = end;
        } else if src[index..].starts_with(b"/*") {
            let mut depth = 0_usize;
            let mut end = index;
            while end < src.len() {
                if src[end..].starts_with(b"/*") {
                    depth += 1;
                    end += 2;
                } else if src[end..].starts_with(b"*/") {
                    depth -= 1;
                    end += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    end += 1;
                }
            }
            let end = end.min(src.len());
            blank(&mut code, index..end);
            index = end;
        } else if let Some(hashes) = raw_string_hashes(src, index) {
            let start = (index + hashes + 2).min(src.len());
            let mut closing = vec![b'"'];
            closing.extend(std::iter::repeat(b'#').take(hashes));
            let end = src[start..]
                .windows(closing.len())
                .position(|window| window == closing.as_slice())
                .map_or(src.len(), |offset| start + offset);
            blank(&mut code, start..end);
            index = (end + closing.len()).min(src.len());
        } else if src[index] == b'"' {
            let mut end = index + 1;
            while end < src.len() && src[end] != b'"' {
                end += if src[end] == b'\\' { 2 } else { 1 };
            }
            let end = end.min(src.len());
            blank(&mut code, index + 1..end);
            index = (end + 1).min(src.len());
        } else if src[index] == b'\'' {
            let close = if src.get(index + 1) == Some(&b'\\') {
                src[(index + 3).min(src.len())..]
                    .iter()
                    .position(|byte| *byte == b'\'')
                    .map(|offset| index + 3 + offset)
            } else {
                character_literal_end(src, index)
            };
            if let Some(close) = close {
                blank(&mut code, index + 1..close);
                index = close + 1;
            } else {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    code
}

/// The offset of the `'` that closes the character literal opening at
/// `index`, when there is one.
///
/// A character literal holds exactly one character, so the closing quote is
/// right after it. A lifetime (`'a`) has no closing quote there, and is left
/// alone.
fn character_literal_end(src: &[u8], index: usize) -> Option<usize> {
    let first = *src.get(index + 1)?;
    let len = utf8_char_len(first);
    let close = index + 1 + len;
    (src.get(close) == Some(&b'\'')).then_some(close)
}

/// How many bytes the UTF-8 character starting with `first` takes.
fn utf8_char_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

/// At the `r` of a raw string (`r"…"`, `r#"…"#`, `br"…"`), how many `#` it
/// uses; `None` anywhere else, including a raw identifier such as `r#type`.
fn raw_string_hashes(src: &[u8], at: usize) -> Option<usize> {
    if src.get(at) != Some(&b'r') {
        return None;
    }
    let starts_a_token = match at.checked_sub(1).map(|before| src[before]) {
        None => true,
        Some(b'b') => at < 2 || !is_ident_byte(src[at - 2]),
        Some(before) => !is_ident_byte(before),
    };
    if !starts_a_token {
        return None;
    }
    let mut hashes = 0_usize;
    while src.get(at + 1 + hashes) == Some(&b'#') {
        hashes += 1;
    }
    (src.get(at + 1 + hashes) == Some(&b'"')).then_some(hashes)
}

/// Blank a byte range to spaces, keeping the newlines.
fn blank(code: &mut [u8], range: std::ops::Range<usize>) {
    for byte in &mut code[range] {
        if *byte != b'\n' {
            *byte = b' ';
        }
    }
}

/// A single shell script that handles every program name in `$0`. Logs every
/// call, writes a marker on restart, and answers the queries the production
/// code sends so the test takes the "active / will restart" branch when the
/// guard is removed (the mutation scenario).
const FAKE_SERVICE_MANAGER_SCRIPT: &str = r#"#!/bin/sh
PROG="$(basename "$0")"
DIR="$(dirname "$0")"
MARKER="$DIR/$PROG.restarted.marker"
LOG="$DIR/$PROG.log"

echo "$@" >> "$LOG"

case "$PROG" in
  systemctl)
    # Strip option flags so the first positional arg is the action.
    ACTION=""
    for arg in "$@"; do
      case "$arg" in
        --user|--no-block|--no-pager) continue ;;
      esac
      ACTION="$arg"
      break
    done
    case "$ACTION" in
      restart) echo "restarted $(date +%s.%N)" >> "$MARKER"; exit 0 ;;
      is-active) echo "active"; exit 0 ;;
      status) echo "active"; exit 0 ;;
      cat) cat <<'EOF'
[Unit]
Description=test
EOF
        exit 0 ;;
    esac
    exit 0
    ;;
  launchctl)
    case "$1" in
      list) exit 0 ;;
    esac
    exit 0
    ;;
  rc-service)
    case "$2" in
      status) echo "started"; exit 0 ;;
      restart) echo "restarted $(date +%s.%N)" >> "$MARKER"; exit 0 ;;
    esac
    exit 0
    ;;
  rc-update|schtasks)
    exit 0
    ;;
esac
exit 0
"#;
