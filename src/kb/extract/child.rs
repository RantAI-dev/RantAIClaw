//! The child side of an inbound document extraction.
//!
//! Bytes a sender chose are parsed **here**, in a process of its own. The
//! parser sees a document and nothing else: no environment, no
//! configuration, no data directory. Whatever it does — panic, abort, loop,
//! allocate — the agent process keeps running, answers the sender with a
//! visible rejection, and the next document can still be read.
//!
//! The boundary is a **crash boundary, not a privilege boundary**: the child
//! keeps the agent's own filesystem and network reach. Its environment holds
//! nothing but the request, but a parser that wanted to read an operator
//! file or open a socket could.
//!
//! The parent starts this code as the same binary with a hidden subcommand
//! ([`HIDDEN_SUBCOMMAND`]). The request travels in the environment, the
//! document in stdin, the answer in stdout as one line of JSON behind a
//! sentinel.
//!
//! Nothing in this module reads the CLI, the configuration or the data
//! directory, and nothing here opens a socket: the hidden subcommand is not
//! an endpoint.

use std::ffi::OsStr;
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

use crate::kb::extract::inbound::{InboundDocumentKind, InboundExtraction};
use crate::kb::{KbError, KbResult};

/// The hidden subcommand the parent starts the child with.
///
/// It is hidden from `--help` and it is not an endpoint: it reads stdin, it
/// writes one frame to stdout, and it exits.
pub const HIDDEN_SUBCOMMAND: &str = "__extract-document";

/// The reason a failure the child did not write is reported with.
///
/// A [`KbError`] variant this module does not name would carry a text of its
/// own — a path, a response body, a parser's words — and the reason reaches
/// the model. So such a failure is reported with a sentence of ours.
const UNEXPECTED_REASON: &str = "the document could not be read";

/// The sentinel that marks the child's answer on stdout.
///
/// The answer is the sentinel, a newline, and one line of JSON. A JSON
/// string never carries a raw newline (serde escapes it as `\n`), so a
/// document's own text cannot forge a second frame boundary.
pub const SENTINEL: &str = "RANTAICLAW_EXTRACT_RESULT_V1";

/// The environment variable naming the document kind to read.
pub const KIND_VAR: &str = "RANTAICLAW_EXTRACT_KIND";

/// The environment variable carrying the page cap.
pub const MAX_PAGES_VAR: &str = "RANTAICLAW_EXTRACT_MAX_PAGES";

/// The environment variable carrying the character cap.
pub const MAX_CHARS_VAR: &str = "RANTAICLAW_EXTRACT_MAX_CHARS";

/// The environment variable that marks "this process is the child".
///
/// The production child is recognised by its hidden subcommand; the test
/// fixture is recognised by this variable, because the test harness cannot
/// be given a subcommand of its own.
pub const CHILD_VAR: &str = "RANTAICLAW_EXTRACT_CHILD";

/// The environment variable that says how long the test reader sleeps.
#[cfg(test)]
pub(crate) const SLEEP_SECS_VAR: &str = "RANTAICLAW_EXTRACT_SLEEP_SECS";

/// The environment variable the test reader reports back.
#[cfg(test)]
pub(crate) const PROBE_VAR: &str = "RANTAICLAW_EXTRACT_PROBE";

/// The most bytes the child reads from stdin.
///
/// The parent caps the document long before this; the cap here is the
/// backstop for a caller that does not.
const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// What the reader saw instead of a value.
#[cfg(test)]
const ABSENT: &str = "<absent>";

/// The caps the child applies while it parses.
///
/// The deadline is not one of them: the parent owns the deadline and kills
/// the child when it passes, so the child has no clock of its own to keep.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChildLimits {
    /// The most pages a PDF may have. Counted before the text is extracted.
    pub(crate) max_pages: u32,
    /// The most characters of text that leave the extractor.
    pub(crate) max_chars: usize,
}

/// What kind of failure the child reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ErrorKind {
    /// The parser failed. The parent rebuilds a [`KbError::Extraction`].
    Extraction,
    /// This build cannot read this kind of document at all. The parent
    /// rebuilds a [`KbError::UnsupportedFileType`].
    Unsupported,
    /// Anything else. The parent rebuilds a [`KbError::Other`].
    Other,
}

/// One answer from the child: the whole wire protocol.
///
/// A success carries the text, the page count and whether the text was cut.
/// A failure carries a kind, so the parent can rebuild the same
/// [`KbError`] the child would have returned.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ChildFrame {
    /// Whether the extraction succeeded.
    pub ok: bool,
    /// The extracted text, on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The page count, on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pages: Option<u32>,
    /// Whether the text was cut, on success.
    #[serde(default)]
    pub truncated: bool,
    /// The kind of failure, on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ErrorKind>,
    /// The extractor that failed, on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extractor: Option<String>,
    /// The failure message, on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ChildFrame {
    /// The frame for a successful extraction.
    fn success(extraction: &InboundExtraction) -> Self {
        Self {
            ok: true,
            text: Some(extraction.text.clone()),
            pages: extraction.pages,
            truncated: extraction.truncated,
            ..Self::default()
        }
    }

    /// The frame for a failure, keeping the kind so the parent can rebuild
    /// the same error.
    fn failure(error: &KbError) -> Self {
        let (kind, extractor, message) = match error {
            KbError::Extraction { extractor, message } => (
                ErrorKind::Extraction,
                Some(extractor.clone()),
                message.clone(),
            ),
            KbError::UnsupportedFileType(message) => {
                (ErrorKind::Unsupported, None, message.clone())
            }
            KbError::Other(message) => (ErrorKind::Other, None, message.clone()),
            // Every other variant carries a text of its own — a path, a
            // response body, a parser's words — and the reason reaches the
            // model. So the reason is ours, whatever the variant was.
            _ => (ErrorKind::Other, None, UNEXPECTED_REASON.to_string()),
        };
        Self {
            ok: false,
            kind: Some(kind),
            extractor,
            message: Some(message),
            ..Self::default()
        }
    }
}

/// The whole child: read the request from the environment, read the document
/// from stdin, write the frame to stdout, return the exit code.
///
/// The caller is the hidden subcommand, dispatched before the CLI is parsed
/// and before any configuration, profile, log or data directory is touched.
/// The exit code is 0 for a document that was read and 1 for one that was
/// refused; a parser that panics never reaches the return, and the process
/// dies with it — that is the point of the boundary.
pub fn run_child_from_env() -> i32 {
    let (frame, code) = match child_outcome() {
        Ok(extraction) => (ChildFrame::success(&extraction), 0),
        Err(error) => (ChildFrame::failure(&error), 1),
    };
    if write_frame(&frame).is_err() {
        return 1;
    }
    code
}

/// Whether these arguments ask for the hidden extraction subcommand.
///
/// The child is the same binary with a hidden subcommand. Only the first
/// argument may ask for it; nothing else may.
///
/// The arguments are the ones **after** the program name, so the first
/// argument is the first word of the command line: `rantaiclaw
/// __extract-document` asks for the child and `rantaiclaw chat
/// __extract-document` does not.
pub fn is_child_invocation<I, T>(args: I) -> bool
where
    I: IntoIterator<Item = T>,
    T: AsRef<OsStr>,
{
    args.into_iter()
        .next()
        .is_some_and(|arg| arg.as_ref() == OsStr::new(HIDDEN_SUBCOMMAND))
}

/// The child's exit code when this process is the child, `None` otherwise.
///
/// The binary calls this first, before it parses the command line and
/// before it reads the configuration, the profile, the log or the data
/// directory: the child must see none of them.
pub fn child_exit_code_if_invoked() -> Option<i32> {
    // The program name is the zeroth argument, so the first word of the
    // command line is the first argument after it.
    if !is_child_invocation(std::env::args_os().skip(1)) {
        return None;
    }
    Some(run_child_from_env())
}

/// Read the request, read the document, parse it.
fn child_outcome() -> KbResult<InboundExtraction> {
    let request = read_request()?;
    let bytes = read_input()?;
    run_extraction(&request.kind, &bytes, request.limits)
}

/// What the parent asked the child to read.
struct ChildRequest {
    /// The kind of document.
    kind: String,
    /// The caps to parse under.
    limits: ChildLimits,
}

/// The request, read from the environment the parent set.
fn read_request() -> KbResult<ChildRequest> {
    let kind = std::env::var(KIND_VAR)
        .map_err(|_| KbError::Other("the document kind is missing".to_string()))?;
    let max_pages = std::env::var(MAX_PAGES_VAR)
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| KbError::Other("the page limit is missing".to_string()))?;
    let max_chars = std::env::var(MAX_CHARS_VAR)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .ok_or_else(|| KbError::Other("the character limit is missing".to_string()))?;
    Ok(ChildRequest {
        kind,
        limits: ChildLimits {
            max_pages,
            max_chars,
        },
    })
}

/// The document, read from stdin.
fn read_input() -> KbResult<Vec<u8>> {
    let limit = usize::try_from(MAX_INPUT_BYTES).unwrap_or(usize::MAX);
    let stdin = std::io::stdin();
    let mut handle = stdin.lock().take(MAX_INPUT_BYTES.saturating_add(1));
    let mut bytes = Vec::new();
    handle
        .read_to_end(&mut bytes)
        .map_err(|_| KbError::Other("the document could not be read".to_string()))?;
    if bytes.len() > limit {
        return Err(KbError::Other(
            "the document is too large to read".to_string(),
        ));
    }
    Ok(bytes)
}

/// Parse the document the way the kind says.
fn run_extraction(kind: &str, bytes: &[u8], limits: ChildLimits) -> KbResult<InboundExtraction> {
    match kind {
        "pdf" => extract_blocking(InboundDocumentKind::Pdf, bytes, limits),
        "docx" => extract_blocking(InboundDocumentKind::Docx, bytes, limits),
        "text" => extract_blocking(InboundDocumentKind::Text, bytes, limits),
        #[cfg(test)]
        "sleep" => test_sleep(),
        #[cfg(test)]
        "alloc" => test_alloc(),
        #[cfg(test)]
        "env" => test_env(),
        #[cfg(test)]
        "pid" => test_pid(),
        other => Err(KbError::Other(format!("unknown document kind '{other}'"))),
    }
}

/// Write the frame to stdout: the sentinel, a newline, the JSON.
fn write_frame(frame: &ChildFrame) -> std::io::Result<()> {
    let json = serde_json::to_string(frame)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut out = std::io::stdout().lock();
    out.write_all(SENTINEL.as_bytes())?;
    out.write_all(b"\n")?;
    out.write_all(json.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// The extraction itself, on the child's own thread.
pub(crate) fn extract_blocking(
    kind: InboundDocumentKind,
    bytes: &[u8],
    limits: ChildLimits,
) -> KbResult<InboundExtraction> {
    let (text, pages) = match kind {
        InboundDocumentKind::Pdf => extract_pdf(bytes, limits)?,
        InboundDocumentKind::Docx => (extract_docx(bytes)?, None),
        InboundDocumentKind::Text => (extract_text(bytes)?, None),
    };
    let (text, truncated) = cut_text(text, limits.max_chars);
    Ok(InboundExtraction {
        text,
        pages,
        truncated,
    })
}

/// A PDF's text, with the page count checked **before** the text is read.
///
/// The count comes from the bytes, so a 500-page file is refused without its
/// text ever being parsed.
fn extract_pdf(bytes: &[u8], limits: ChildLimits) -> KbResult<(String, Option<u32>)> {
    // A parser's own error can quote the document — a dictionary key, an
    // outline destination, a value it read — and this reason reaches the
    // model. The reason is ours; the parser's words are not.
    let pages =
        crate::kb::extract::pdf_splitter::page_count(bytes).map_err(|_| KbError::Extraction {
            extractor: "inbound-pdf".into(),
            message: "the PDF could not be read".into(),
        })?;
    if pages > limits.max_pages {
        return Err(KbError::Extraction {
            extractor: "inbound-pdf".into(),
            message: format!(
                "the document has {pages} pages, over the {}-page limit",
                limits.max_pages
            ),
        });
    }
    let text = pdf_extract::extract_text_from_mem(bytes).map_err(|_| KbError::Extraction {
        extractor: "inbound-pdf".into(),
        message: "the PDF text layer could not be read".into(),
    })?;
    Ok((text, Some(pages)))
}

/// A DOCX's text.
///
/// The office processor lives behind the `kb-office` feature. A build without
/// it says so, rather than dropping the document.
#[cfg(feature = "kb-office")]
fn extract_docx(bytes: &[u8]) -> KbResult<String> {
    // The display name is fixed: the sender chose the file name, and it does
    // not belong in an error message. The office processor's own message is
    // dropped for the same reason: it can quote a value it read out of the
    // document's XML.
    crate::kb::file::office::process_docx(bytes, "inbound document").map_err(|_| {
        KbError::Extraction {
            extractor: "inbound-docx".into(),
            message: "the DOCX document could not be read".into(),
        }
    })
}

/// A DOCX's text, in a build without the office processor.
#[cfg(not(feature = "kb-office"))]
fn extract_docx(_bytes: &[u8]) -> KbResult<String> {
    Err(KbError::UnsupportedFileType(
        "reading DOCX documents needs the kb-office feature".into(),
    ))
}

/// A plain text file's text: UTF-8, and nothing that is not text.
fn extract_text(bytes: &[u8]) -> KbResult<String> {
    let text = std::str::from_utf8(bytes).map_err(|_| KbError::Extraction {
        extractor: "inbound-text".into(),
        message: "the file is not valid UTF-8".into(),
    })?;
    if text.contains('\0') {
        return Err(KbError::Extraction {
            extractor: "inbound-text".into(),
            message: "the file contains NUL bytes and is not a text document".into(),
        });
    }
    Ok(text.to_string())
}

/// The text, cut at the limit, and whether it was cut.
///
/// A cut is never silent: the caller turns the flag into a note the sender
/// sees, so the model is never handed a document that looks complete and is
/// not.
pub(crate) fn cut_text(text: String, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text, false);
    }
    (text.chars().take(max_chars).collect(), true)
}

/// A reader that sleeps, so the parent's deadline can be tested.
///
/// Test-only: a production build has no such kind.
#[cfg(test)]
fn test_sleep() -> KbResult<InboundExtraction> {
    let secs = std::env::var(SLEEP_SECS_VAR)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60);
    std::thread::sleep(std::time::Duration::from_secs(secs));
    Ok(InboundExtraction {
        text: String::new(),
        pages: None,
        truncated: false,
    })
}

/// A reader that asks for more memory than the ceiling allows.
///
/// The allocation fails under the ceiling and the process dies with it: the
/// agent must not be the one that dies. Test-only.
#[cfg(test)]
fn test_alloc() -> KbResult<InboundExtraction> {
    let mut huge: Vec<u8> = Vec::with_capacity(2 * 1024 * 1024 * 1024);
    huge.push(0);
    Ok(InboundExtraction {
        text: String::new(),
        pages: None,
        truncated: false,
    })
}

/// A reader that reports what the environment looks like from in there.
///
/// Test-only.
#[cfg(test)]
fn test_env() -> KbResult<InboundExtraction> {
    let probe = std::env::var(PROBE_VAR).unwrap_or_else(|_| ABSENT.to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| ABSENT.to_string());
    Ok(InboundExtraction {
        text: format!("probe={probe} home={home}"),
        pages: None,
        truncated: false,
    })
}

/// A reader that reports its own process id.
///
/// Test-only.
#[cfg(test)]
fn test_pid() -> KbResult<InboundExtraction> {
    Ok(InboundExtraction {
        text: std::process::id().to_string(),
        pages: None,
        truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── The text rules ──────────────────────────────────────────────────────

    #[test]
    fn text_over_the_limit_is_cut_and_says_so() {
        let (text, truncated) = cut_text("abcdef".repeat(10), 25);
        assert_eq!(text.chars().count(), 25);
        assert!(truncated);
    }

    #[test]
    fn text_under_the_limit_is_left_alone() {
        let (text, truncated) = cut_text("hello".to_string(), 25);
        assert_eq!(text, "hello");
        assert!(!truncated);
    }

    #[test]
    fn a_file_that_is_not_utf8_is_refused() {
        let err = extract_text(&[0xff, 0xfe, 0x00]).expect_err("not text");
        assert!(err.to_string().contains("UTF-8"), "{err}");
    }

    #[test]
    fn a_file_with_a_nul_byte_is_refused() {
        let err = extract_text(b"hello\0world").expect_err("not text");
        assert!(err.to_string().contains("NUL"), "{err}");
    }

    /// A failure the child did not write is reported with our own words.
    ///
    /// The variants this module does not name carry a text of their own — a
    /// path, a response body, a parser's words — and the reason reaches the
    /// model. The reason is therefore ours, whatever the variant was.
    #[test]
    fn an_unexpected_reason_is_replaced_by_ours() {
        let frame = ChildFrame::failure(&KbError::Io(std::io::Error::other(
            "/home/operator/.rantaiclaw/secrets.env",
        )));
        let message = frame.message.expect("a reason");
        assert_eq!(message, "the document could not be read");
        assert!(
            !message.contains("secrets.env"),
            "the reason must not carry the variant's own text: {message}"
        );
    }

    // ── The hidden subcommand ───────────────────────────────────────────────

    /// The hidden subcommand is the child.
    #[test]
    fn the_hidden_subcommand_is_the_child() {
        assert!(is_child_invocation([HIDDEN_SUBCOMMAND]));
    }

    /// Another command is not the child.
    #[test]
    fn another_command_is_not_the_child() {
        assert!(!is_child_invocation(["chat"]));
    }

    /// No command is not the child either.
    #[test]
    fn no_command_is_not_the_child() {
        assert!(!is_child_invocation(Vec::<String>::new()));
        assert!(!is_child_invocation([""]));
    }

    /// The hidden subcommand is only honoured as the **first** argument.
    ///
    /// `rantaiclaw chat __extract-document` is the chat command with a word
    /// behind it, not a child: the hidden subcommand is never a way in.
    #[test]
    fn the_hidden_subcommand_is_only_honoured_as_the_first_argument() {
        assert!(!is_child_invocation(["chat", HIDDEN_SUBCOMMAND]));
        assert!(!is_child_invocation([
            "--profile",
            "default",
            HIDDEN_SUBCOMMAND
        ]));
    }

    /// An ordinary run is not the child.
    ///
    /// This is what the binary's dispatch rests on: a run that did not ask
    /// for the hidden subcommand must go on to the CLI.
    #[test]
    fn an_ordinary_run_is_not_the_child() {
        assert!(child_exit_code_if_invoked().is_none());
    }

    // ── The child entry point ───────────────────────────────────────────────

    /// The child, as a test.
    ///
    /// The production child is the same binary started with the hidden
    /// subcommand. A test harness cannot be given a subcommand of its own,
    /// so this fixture stands in for it: the parent starts the **test
    /// binary** with this one test selected, and the fixture does the
    /// child's work.
    ///
    /// It returns immediately unless the parent marked the process as the
    /// child, so an ordinary test run never blocks on stdin.
    #[test]
    fn extract_child_entry_fixture() {
        if std::env::var(CHILD_VAR).ok().as_deref() != Some("1") {
            return;
        }
        // A reader that floods stdout: the parent's answer cap has to stop
        // it, and the parent has to notice without waiting for a reader that
        // is still writing.
        if std::env::var(KIND_VAR).ok().as_deref() == Some("flood") {
            flood_stdout();
            std::process::exit(1);
        }
        let code = run_child_from_env();
        std::process::exit(code);
    }

    /// A reader that writes far more than the answer cap allows, and then
    /// keeps the pipe open.
    ///
    /// Test-only: the point is a reader that is still running when the cap
    /// is passed.
    #[cfg(test)]
    fn flood_stdout() {
        let mut out = std::io::stdout().lock();
        let block = vec![b'x'; 64 * 1024];
        for _ in 0..64 {
            if std::io::Write::write_all(&mut out, &block).is_err() {
                return;
            }
        }
        let _ = std::io::Write::flush(&mut out);
        // The pipe is full by now, and this reader is alive until it is
        // killed.
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}
