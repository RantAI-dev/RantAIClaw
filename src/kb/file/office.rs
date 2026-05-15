//! Office-document processor — feature-gated behind `kb-office`.
//!
//! Supported extensions:
//! - `.docx` → walk paragraphs via `docx-rs`, join with `\n\n`
//! - `.xlsx` / `.xls` / `.ods` → read every sheet via `calamine`, emit
//!   `## Sheet: <name>` headers followed by TSV-formatted rows
//!
//! Deferred (Phase-7+ work — fail-fast `UnsupportedFileType` for now):
//!   `.pptx`, `.rtf`, `.epub`, `.doc`, `.ppt`, `.odt`, `.gltf`, `.glb`.
//!
//! The TS source funnels these through `@/lib/files/parsers` which wraps
//! mammoth, xlsx-js, etc. We'll add backends one extension at a time
//! rather than committing partial support that silently mangles content.
#![cfg(feature = "kb-office")]

use std::fmt::Write as _;
use std::path::Path;

use calamine::{Data, Reader};
use docx_rs::{DocumentChild, ParagraphChild, RunChild};

use crate::kb::{KbError, KbResult};

/// Entry point — dispatches by lowercased extension.
pub async fn process_office(path: &Path) -> KbResult<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    // Read once on the async runtime; the actual parsers are sync and run
    // on the calling thread (small documents, single-shot).
    let bytes = tokio::fs::read(path).await?;
    let display = path.display().to_string();

    match ext.as_str() {
        "docx" => process_docx(&bytes, &display),
        "xlsx" | "xls" | "ods" => process_spreadsheet(&bytes, &ext, &display),
        // TODO(kb-office-extensions): add backends for these one at a time
        // (mammoth/pdf-based pptx, rtf via stripped formatting, epub via
        // zip+xml, gltf via tinygltf). Surfacing UnsupportedFileType keeps
        // ingest deterministic for now.
        "pptx" | "rtf" | "epub" | "doc" | "ppt" | "odt" | "gltf" | "glb" => {
            Err(KbError::UnsupportedFileType(format!(
                "office extension not yet supported: .{ext} ({display})"
            )))
        }
        _ => Err(KbError::UnsupportedFileType(format!(
            "unrecognized office extension: .{ext} ({display})"
        ))),
    }
}

/// Walk the docx document tree, concatenating paragraph text. Joins
/// paragraphs with double-newline so the downstream chunker sees natural
/// boundaries. Empty paragraphs are skipped to avoid burning chunk budget.
fn process_docx(bytes: &[u8], display: &str) -> KbResult<String> {
    let docx = docx_rs::read_docx(bytes).map_err(|e| KbError::Extraction {
        extractor: "docx-rs".into(),
        message: format!("read_docx failed for {display}: {e:?}"),
    })?;

    let mut paragraphs: Vec<String> = Vec::with_capacity(docx.document.children.len());
    for child in &docx.document.children {
        if let DocumentChild::Paragraph(p) = child {
            let mut buf = String::new();
            for child in &p.children {
                if let ParagraphChild::Run(run) = child {
                    for rc in &run.children {
                        if let RunChild::Text(t) = rc {
                            buf.push_str(&t.text);
                        }
                    }
                }
            }
            if !buf.is_empty() {
                paragraphs.push(buf);
            }
        }
    }
    Ok(paragraphs.join("\n\n"))
}

/// Read every sheet via calamine, emit `## Sheet: <name>\n` followed by
/// tab-separated rows. The chunker downstream is content-aware and treats
/// the `## ` lines as section starts.
fn process_spreadsheet(bytes: &[u8], ext: &str, display: &str) -> KbResult<String> {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    let mut output = String::new();

    // Each backend has its own concrete reader type, so dispatch with a
    // small match instead of a trait-object dance.
    match ext {
        "xlsx" => {
            let mut wb = calamine::Xlsx::new(cursor).map_err(|e| spreadsheet_err(ext, display, &e))?;
            render_workbook(&mut wb, &mut output)?;
        }
        "xls" => {
            let mut wb = calamine::Xls::new(cursor).map_err(|e| spreadsheet_err(ext, display, &e))?;
            render_workbook(&mut wb, &mut output)?;
        }
        "ods" => {
            let mut wb = calamine::Ods::new(cursor).map_err(|e| spreadsheet_err(ext, display, &e))?;
            render_workbook(&mut wb, &mut output)?;
        }
        other => {
            return Err(KbError::UnsupportedFileType(format!(
                "spreadsheet dispatcher got unexpected ext: {other}"
            )));
        }
    }
    Ok(output)
}

fn spreadsheet_err(ext: &str, display: &str, e: &impl std::fmt::Debug) -> KbError {
    KbError::Extraction {
        extractor: format!("calamine::{ext}"),
        message: format!("open failed for {display}: {e:?}"),
    }
}

fn render_workbook<RS, R>(reader: &mut R, out: &mut String) -> KbResult<()>
where
    RS: std::io::Read + std::io::Seek,
    R: Reader<RS>,
    R::Error: std::fmt::Debug,
{
    let names = reader.sheet_names();
    for name in names {
        let range = reader
            .worksheet_range(&name)
            .map_err(|e| KbError::Extraction {
                extractor: "calamine".into(),
                message: format!("worksheet_range({name}): {e:?}"),
            })?;
        // Header line — `writeln!` into a String is infallible; the
        // expect makes the contract local instead of bubbling fmt::Error
        // through the KbResult signature.
        writeln!(out, "## Sheet: {name}").expect("write to String never fails");
        for row in range.rows() {
            let line = row
                .iter()
                .map(format_cell)
                .collect::<Vec<_>>()
                .join("\t");
            writeln!(out, "{line}").expect("write to String never fails");
        }
        out.push('\n');
    }
    Ok(())
}

fn format_cell(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Float(f) => f.to_string(),
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => dt.to_string(),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#ERR({e:?})"),
    }
}
