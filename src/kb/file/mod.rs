//! Top-level file processor — dispatches by extension to per-type extractors.
//!
//! Port of `src/lib/rag/file-processor.ts` from the Node platform. The four
//! supported types map 1:1 to the TS source:
//!
//! - [`SupportedFileType::Markdown`] — read file verbatim
//! - [`SupportedFileType::Pdf`]      — route through [`crate::kb::extract`]
//! - [`SupportedFileType::Image`]    — OpenRouter vision LLM
//! - [`SupportedFileType::Document`] — office files behind the
//!   `kb-office` feature (docx via `docx-rs`, xlsx/xls/ods via `calamine`)
//! - [`SupportedFileType::Text`]     — read file verbatim (csv, code, json, …)
//!
//! Strictly isolated from [`crate::memory`]: KB ingests org-level documents,
//! agent short-term memory is unrelated.

use std::path::{Path, PathBuf};

use crate::kb::extract::{build_extractor, extract_with_fallback};
use crate::kb::{KbConfig, KbError, KbResult};

pub mod image;
pub mod markdown;
#[cfg(feature = "kb-office")]
pub mod office;
pub mod text;

/// File-type classification driven by extension. Mirrors the TS union
/// `"markdown" | "pdf" | "image" | "document" | "text"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportedFileType {
    Markdown,
    Pdf,
    Image,
    Document,
    Text,
}

/// Document-type hint forwarded to a hypothetical OCR pipeline. Currently
/// only carried through `ProcessingOptions`; consumed once the Ollama OCR
/// port lands in a later phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentTypeHint {
    PrintedText,
    Handwritten,
    Table,
    Form,
    Figure,
    Mixed,
}

/// Output-format hint for OCR / vision flows. Defaults to [`OutputFormat::Markdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    PlainText,
    #[default]
    Markdown,
}

/// Per-call processing options. All fields default to the conservative path
/// (no OCR, markdown output).
#[derive(Debug, Clone, Default)]
pub struct ProcessingOptions {
    /// Opt-in flag for the Ollama-based OCR pipeline. Reserved — the Rust
    /// port currently returns an explicit error when this is `true`. The
    /// Ollama wiring will land in a later phase.
    pub use_ocr_pipeline: bool,
    pub document_type: Option<DocumentTypeHint>,
    pub output_format: Option<OutputFormat>,
}

/// Result of [`process_file`]. The `content` is the extracted text in
/// whatever format the per-type processor produces (typically Markdown).
#[derive(Debug, Clone)]
pub struct ProcessedFile {
    pub content: String,
    pub file_type: SupportedFileType,
    pub original_path: PathBuf,
}

// Extension lists — verbatim port of `file-processor.ts:43-60`.
const MARKDOWN_EXTENSIONS: &[&str] = &[".md", ".markdown"];
const PDF_EXTENSIONS: &[&str] = &[".pdf"];
const IMAGE_EXTENSIONS: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".webp", ".heic"];
// Document extensions actually handled by the `kb-office` processor today.
// Detection must only return Document for these — adding extensions here
// without a working backend would create a silent-fallback bug where
// detection says "yes" and processing then errors with UnsupportedFileType
// (forbidden by CLAUDE.md §3.5 fail-fast contract).
const DOCUMENT_EXTENSIONS: &[&str] = &[".docx", ".xlsx", ".xls", ".ods"];

/// Office-format extensions that are recognized as document types but not
/// yet wired to a processor backend. Kept here so future maintainers can
/// see the deliberate deferral and so user-facing error messages can refer
/// to a single canonical list. NOT included in [`detect_file_type`].
pub const DEFERRED_DOCUMENT_EXTENSIONS: &[&str] =
    &[".pptx", ".rtf", ".epub", ".doc", ".ppt", ".odt", ".gltf", ".glb"];
const TEXT_EXTENSIONS: &[&str] = &[
    ".csv", ".tsv", ".json", ".jsonl", ".html", ".htm", ".xml", ".yaml", ".yml", ".toml", ".py",
    ".ts", ".tsx", ".js", ".jsx", ".go", ".rs", ".java", ".c", ".cpp", ".h", ".rb", ".php", ".sh",
    ".sql", ".r", ".swift", ".kt", ".txt", ".log", ".ini", ".env",
];

/// Detect [`SupportedFileType`] from `path`'s extension. Case-insensitive.
/// Returns `None` for unknown extensions or paths without one.
pub fn detect_file_type(path: &Path) -> Option<SupportedFileType> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    let dotted = format!(".{}", ext.to_ascii_lowercase());
    let ext = dotted.as_str();

    if MARKDOWN_EXTENSIONS.contains(&ext) {
        return Some(SupportedFileType::Markdown);
    }
    if PDF_EXTENSIONS.contains(&ext) {
        return Some(SupportedFileType::Pdf);
    }
    if IMAGE_EXTENSIONS.contains(&ext) {
        return Some(SupportedFileType::Image);
    }
    if DOCUMENT_EXTENSIONS.contains(&ext) {
        return Some(SupportedFileType::Document);
    }
    if TEXT_EXTENSIONS.contains(&ext) {
        return Some(SupportedFileType::Text);
    }
    None
}

/// `true` when [`detect_file_type`] would return `Some`.
pub fn is_supported_file(path: &Path) -> bool {
    detect_file_type(path).is_some()
}

/// Flat list of every supported extension (with leading dot). Useful for
/// callers wiring up directory walkers / UI dropdowns.
pub fn supported_extensions() -> Vec<&'static str> {
    let mut out = Vec::with_capacity(
        MARKDOWN_EXTENSIONS.len()
            + PDF_EXTENSIONS.len()
            + IMAGE_EXTENSIONS.len()
            + DOCUMENT_EXTENSIONS.len()
            + TEXT_EXTENSIONS.len(),
    );
    out.extend_from_slice(MARKDOWN_EXTENSIONS);
    out.extend_from_slice(PDF_EXTENSIONS);
    out.extend_from_slice(IMAGE_EXTENSIONS);
    out.extend_from_slice(DOCUMENT_EXTENSIONS);
    out.extend_from_slice(TEXT_EXTENSIONS);
    out
}

/// Process a single file. Errors on missing path or unsupported extension.
///
/// PDF dispatch goes through [`crate::kb::extract::build_extractor`] using
/// `cfg.extract_primary` as the primary and `cfg.extract_fallback` as the
/// fallback (port of `file-processor.ts:135-139`).
pub async fn process_file(
    cfg: &KbConfig,
    path: &Path,
    opts: ProcessingOptions,
) -> KbResult<ProcessedFile> {
    if !tokio::fs::try_exists(path).await? {
        return Err(KbError::NotFound(format!("file: {}", path.display())));
    }
    let file_type = detect_file_type(path)
        .ok_or_else(|| KbError::UnsupportedFileType(path.display().to_string()))?;

    let content = match file_type {
        SupportedFileType::Markdown => markdown::process_markdown(path).await?,
        SupportedFileType::Pdf => {
            let bytes = tokio::fs::read(path).await?;
            process_pdf(cfg, &bytes, &opts).await?
        }
        SupportedFileType::Image => image::process_image(cfg, path, &opts).await?,
        SupportedFileType::Document => {
            #[cfg(feature = "kb-office")]
            {
                office::process_office(path).await?
            }
            #[cfg(not(feature = "kb-office"))]
            {
                return Err(KbError::UnsupportedFileType(format!(
                    "office files require the kb-office feature: {}",
                    path.display()
                )));
            }
        }
        SupportedFileType::Text => text::process_text(path).await?,
    };

    Ok(ProcessedFile {
        content,
        file_type,
        original_path: path.to_path_buf(),
    })
}

/// Best-effort multi-file driver — failures are logged at `warn` level and
/// skipped so a single bad file doesn't poison a directory ingest. Mirrors
/// `processFiles` in `file-processor.ts:297-313`.
pub async fn process_files(
    cfg: &KbConfig,
    paths: &[PathBuf],
    opts: ProcessingOptions,
) -> Vec<ProcessedFile> {
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        match process_file(cfg, p, opts.clone()).await {
            Ok(processed) => out.push(processed),
            Err(e) => tracing::warn!(path = %p.display(), "process_file failed: {e}"),
        }
    }
    out
}

/// Recursive directory scan — returns only paths whose extension is in
/// [`supported_extensions`]. Silently returns an empty vector if `path`
/// doesn't exist (parity with TS `scanDirectory`).
pub fn scan_directory(path: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    scan_into(path, &mut out);
    out
}

fn scan_into(path: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => scan_into(&p, out),
            Ok(ft) if ft.is_file() && is_supported_file(&p) => out.push(p),
            _ => {}
        }
    }
}

/// Dispatch a PDF through the configured extractor chain. The TS source
/// optionally pre-routes through an Ollama OCR pipeline when
/// `use_ocr_pipeline=true`; that pipeline isn't ported yet, so we return
/// a typed error instead of silently downgrading.
async fn process_pdf(cfg: &KbConfig, bytes: &[u8], opts: &ProcessingOptions) -> KbResult<String> {
    if opts.use_ocr_pipeline {
        // TODO(kb-ocr): port `src/lib/ocr` (Ollama models) in a later phase.
        // For now fail-fast rather than silently fall back to vision-LLM.
        return Err(KbError::Other(
            "OCR pipeline not yet implemented; set use_ocr_pipeline=false".into(),
        ));
    }
    let primary = build_extractor(cfg, &cfg.extract_primary)?;
    let fallback = build_extractor(cfg, &cfg.extract_fallback)?;
    let result = extract_with_fallback(bytes, primary.as_ref(), fallback.as_ref()).await?;
    Ok(result.text)
}
