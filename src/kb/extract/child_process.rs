//! The parent side of an inbound document extraction.
//!
//! The parent parses nothing. It starts the child — the same binary with a
//! hidden subcommand — hands it the document on stdin, reads one frame back
//! and turns that frame into the answer the caller sees.
//!
//! What the child can never see: the environment (it is cleared before the
//! four request variables are set), the configuration, the data directory,
//! the operator's files. What the child can never do: outlive the deadline
//! (it is killed and reaped), take more memory than the ceiling (unix only),
//! or fill the pipe (the read is capped).
//!
//! The memory ceiling is a unix-only guard. On every other target the
//! deadline kill still applies, and nothing else bounds the child's memory.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdout, Command};

use crate::kb::extract::child::{
    self, ChildFrame, ErrorKind, CHILD_VAR, KIND_VAR, MAX_CHARS_VAR, MAX_PAGES_VAR, SENTINEL,
};
use crate::kb::extract::inbound::{InboundExtraction, InboundLimits};
use crate::kb::{KbError, KbResult};

/// The most memory the child may map, in bytes.
///
/// 512 MiB leaves room for a 10 MiB document with a generous text layer and
/// keeps a decompression bomb from taking the machine with it.
pub(crate) const MEMORY_CEILING_BYTES: u64 = 512 * 1024 * 1024;

/// The extra CPU seconds the child may burn past the deadline.
///
/// The deadline kill is the real limit; this is the backstop for a child
/// that burns CPU without ever returning to the wait.
const CPU_BACKSTOP_SECS: u64 = 5;

/// The most bytes a JSON-escaped character costs.
const ESCAPED_CHAR_BYTES: usize = 6;

/// The room the JSON frame gets on top of the escaped text.
const FRAME_SLACK_BYTES: usize = 32 * 1024;

/// The chunk the read loop takes at a time.
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// The name the extractor carries in a rejection.
const EXTRACTOR_NAME: &str = "inbound-document";

/// The whole parent side: start the child, feed it, read the frame.
///
/// `extra_env` is empty in production. It carries the variables of the
/// test-only kinds, which exist in a test build only.
pub(crate) async fn run_child(
    kind: &str,
    extra_env: &[(&str, &str)],
    bytes: Vec<u8>,
    limits: InboundLimits,
    deadline: tokio::time::Instant,
) -> KbResult<InboundExtraction> {
    if !kind_is_known(kind) {
        return Err(rejection("the document kind is not supported"));
    }

    let mut command = child_command();
    // The child starts from nothing: no HOME, no PATH, no provider key, no
    // configuration directory. Only the request travels with it.
    command.env_clear();
    command.env(KIND_VAR, kind);
    command.env(MAX_PAGES_VAR, limits.max_pages.to_string());
    command.env(MAX_CHARS_VAR, limits.max_chars.to_string());
    command.env(CHILD_VAR, "1");
    for (name, value) in extra_env {
        command.env(name, value);
    }
    // stderr can carry document text, so it is discarded, never logged.
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::null());
    // The child dies with the parent, even if this future is dropped.
    command.kill_on_drop(true);
    #[cfg(unix)]
    apply_ceilings(&mut command, limits.timeout);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!("inbound extraction: the reader could not be started: {error}");
            return Err(rejection("the document reader could not be started"));
        }
    };

    let Some(mut stdin) = child.stdin.take() else {
        kill_and_reap(&mut child).await;
        return Err(rejection("the document reader could not be started"));
    };
    let Some(mut stdout) = child.stdout.take() else {
        kill_and_reap(&mut child).await;
        return Err(rejection("the document reader could not be started"));
    };

    // The document goes in while the answer comes out: a 10 MiB document is
    // far larger than a pipe buffer, so writing it first would deadlock.
    let feeder = tokio::spawn(async move {
        let _ = stdin.write_all(&bytes).await;
        let _ = stdin.shutdown().await;
    });

    let cap = limits
        .max_chars
        .saturating_mul(ESCAPED_CHAR_BYTES)
        .saturating_add(FRAME_SLACK_BYTES);
    let outcome = tokio::time::timeout_at(deadline, async {
        // A reader whose answer is too large is still running and still
        // writing: waiting for it would fill the pipe and wait for the
        // deadline instead of refusing the answer. The read failure is the
        // answer, and the child is killed below.
        let frame = match read_capped(&mut stdout, cap).await {
            Ok(frame) => frame,
            Err(failure) => return Err(failure),
        };
        let status = child.wait().await.map_err(|_| ReadFailure::Io)?;
        Ok((frame, status))
    })
    .await;
    // The child is done with the document by now; the writer has nothing
    // left to say.
    feeder.abort();

    match outcome {
        Ok(Ok((frame_bytes, status))) => finish(frame_bytes, status),
        Ok(Err(ReadFailure::TooLarge)) => {
            kill_and_reap(&mut child).await;
            Err(rejection(
                "the document reader sent more than the allowed size",
            ))
        }
        Ok(Err(ReadFailure::Io)) => {
            kill_and_reap(&mut child).await;
            Err(rejection("the document could not be read"))
        }
        Err(_) => {
            kill_and_reap(&mut child).await;
            Err(rejection("the document took too long to read"))
        }
    }
}

/// The command that starts the child.
///
/// In production it is the running binary with the hidden subcommand. In a
/// test build it is the test binary with the child's fixture selected, so
/// the test needs no hook in `main`.
fn child_command() -> Command {
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| std::path::PathBuf::from(child::HIDDEN_SUBCOMMAND));
    let mut command = Command::new(exe);
    #[cfg(test)]
    command.args([
        "--exact",
        "kb::extract::child::tests::extract_child_entry_fixture",
        "--nocapture",
    ]);
    #[cfg(not(test))]
    command.arg(child::HIDDEN_SUBCOMMAND);
    command
}

/// The memory and CPU ceilings, applied before the child runs any code.
#[cfg(unix)]
fn apply_ceilings(command: &mut Command, timeout: Duration) {
    let memory = libc::rlim_t::try_from(MEMORY_CEILING_BYTES).unwrap_or(libc::rlim_t::MAX);
    let cpu = libc::rlim_t::try_from(timeout.as_secs().saturating_add(CPU_BACKSTOP_SECS))
        .unwrap_or(libc::rlim_t::MAX);
    // SAFETY: the closure runs in the forked child, before exec. It sets the
    // child's own limits and touches nothing else.
    unsafe {
        command.pre_exec(move || {
            let memory_limit = libc::rlimit {
                rlim_cur: memory,
                rlim_max: memory,
            };
            if libc::setrlimit(libc::RLIMIT_AS, std::ptr::from_ref(&memory_limit)) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let cpu_limit = libc::rlimit {
                rlim_cur: cpu,
                rlim_max: cpu,
            };
            if libc::setrlimit(libc::RLIMIT_CPU, std::ptr::from_ref(&cpu_limit)) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Why the answer could not be read.
enum ReadFailure {
    /// The child sent more than the cap allows.
    TooLarge,
    /// The pipe broke.
    Io,
}

/// Read the child's answer, capped.
///
/// The cap is the frame: the escaped text plus the frame around it. A child
/// that sends more than that is not answering with a document, and the read
/// stops there instead of filling the agent's memory.
async fn read_capped(stdout: &mut ChildStdout, cap: usize) -> Result<Vec<u8>, ReadFailure> {
    let mut collected: Vec<u8> = Vec::new();
    let mut chunk = vec![0_u8; READ_CHUNK_BYTES];
    loop {
        let read = stdout.read(&mut chunk).await.map_err(|_| ReadFailure::Io)?;
        if read == 0 {
            break;
        }
        if collected.len().saturating_add(read) > cap {
            return Err(ReadFailure::TooLarge);
        }
        collected.extend_from_slice(&chunk[..read]);
    }
    Ok(collected)
}

/// Turn the frame into the answer the caller sees.
fn finish(frame_bytes: Vec<u8>, status: ExitStatus) -> KbResult<InboundExtraction> {
    let Some(frame) = parse_frame(&frame_bytes) else {
        tracing::warn!("inbound extraction: the reader left no readable frame ({status})");
        return Err(rejection("the document could not be read"));
    };
    if frame.ok {
        if !status.success() {
            tracing::warn!("inbound extraction: the reader answered and then failed ({status})");
            return Err(rejection("the document could not be read"));
        }
        return Ok(InboundExtraction {
            text: frame.text.unwrap_or_default(),
            pages: frame.pages,
            truncated: frame.truncated,
        });
    }
    Err(error_from_frame(&frame))
}

/// The frame the child left on stdout, if there is one.
///
/// The sentinel is searched as the byte sequence `SENTINEL + "\n"`, and the
/// **last** occurrence wins: the test harness prints its own noise, and a
/// document's text cannot carry a raw newline, so no other boundary can be
/// forged.
fn parse_frame(bytes: &[u8]) -> Option<ChildFrame> {
    let needle = format!("{SENTINEL}\n");
    let needle = needle.as_bytes();
    if bytes.len() < needle.len() {
        return None;
    }
    let start = bytes
        .windows(needle.len())
        .enumerate()
        .rev()
        .find(|(_, window)| *window == needle)
        .map(|(index, _)| index + needle.len())?;
    // A stream reader takes the first JSON value and ignores whatever the
    // harness printed after it.
    let mut stream =
        serde_json::Deserializer::from_slice(&bytes[start..]).into_iter::<ChildFrame>();
    stream.next()?.ok()
}

/// Rebuild the error the child reported.
fn error_from_frame(frame: &ChildFrame) -> KbError {
    let message = frame
        .message
        .clone()
        .unwrap_or_else(|| "the document could not be read".to_string());
    match frame.kind {
        Some(ErrorKind::Extraction) => KbError::Extraction {
            extractor: frame
                .extractor
                .clone()
                .unwrap_or_else(|| EXTRACTOR_NAME.to_string()),
            message,
        },
        Some(ErrorKind::Unsupported) => KbError::UnsupportedFileType(message),
        _ => KbError::Other(message),
    }
}

/// A rejection the sender can read.
///
/// The reason is a short sentence: it never carries a parser's internals, a
/// file name or a path.
fn rejection(message: &str) -> KbError {
    KbError::Extraction {
        extractor: EXTRACTOR_NAME.to_string(),
        message: message.to_string(),
    }
}

/// Kill the child and wait for it, so no zombie is left behind and the
/// one-at-a-time slot is free again.
async fn kill_and_reap(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Whether the parent knows this kind of document.
fn kind_is_known(kind: &str) -> bool {
    match kind {
        "pdf" | "docx" | "text" => true,
        #[cfg(test)]
        "sleep" | "alloc" | "env" | "pid" | "flood" => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::extract::child::{PROBE_VAR, SLEEP_SECS_VAR};
    use crate::kb::extract::inbound::{
        extract_inbound_document, extract_test_document, fixture_pdf, hostile_fixture_pdf,
        InboundDocumentKind,
    };

    /// The limits the tests run under.
    fn limits(max_pages: u32, max_chars: usize, timeout: Duration) -> InboundLimits {
        InboundLimits {
            max_pages,
            max_chars,
            timeout,
        }
    }

    /// A rejection the sender can read: a sentence, not an internal dump.
    fn assert_clean_rejection(error: &KbError) {
        let message = error.to_string();
        assert!(!message.is_empty(), "a rejection must say something");
        for leak in [
            "panicked",
            "unexpected encoding",
            "RUST_BACKTRACE",
            "backtrace",
            ".pdf",
            ".docx",
        ] {
            assert!(
                !message.contains(leak),
                "the rejection leaks {leak}: {message}"
            );
        }
    }

    // ── The parser cannot take the agent with it ───────────────────────────

    /// A PDF that panics the parser is a rejection, and the next document is
    /// still read.
    ///
    /// `pdf-extract` panics on a font whose `/Encoding` is a name it does
    /// not know. In the release profile that panic aborts the process, so
    /// before the child process existed one small PDF from any allowed sender
    /// stopped the agent.
    #[tokio::test]
    async fn a_pdf_that_makes_the_parser_panic_is_refused_and_the_next_pdf_is_read() {
        let err = extract_inbound_document(
            InboundDocumentKind::Pdf,
            hostile_fixture_pdf(1, "Hello from a PDF"),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect_err("the hostile PDF is refused");
        assert_clean_rejection(&err);

        // The agent is still running: the next PDF is read.
        let extraction = extract_inbound_document(
            InboundDocumentKind::Pdf,
            fixture_pdf(1, "Hello from a PDF"),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect("the next PDF is read");
        assert!(
            extraction.text.contains("Hello from a PDF"),
            "{}",
            extraction.text
        );
    }

    /// The fixture really is hostile: the parser panics on it in process.
    #[test]
    #[should_panic(expected = "unexpected encoding")]
    fn the_hostile_pdf_fixture_panics_the_parser() {
        let _ = pdf_extract::extract_text_from_mem(&hostile_fixture_pdf(1, "Hello from a PDF"));
    }

    /// A refused document is refused with **our** words.
    ///
    /// A parser's error can quote what it read out of the document — a value,
    /// a key, a fragment — and the reason reaches the model. The reason is
    /// therefore a sentence of ours, never the parser's own text.
    #[tokio::test]
    async fn a_refused_document_carries_our_own_words() {
        let err = extract_inbound_document(
            InboundDocumentKind::Pdf,
            b"%PDF-1.4\nthis is not a pdf".to_vec(),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect_err("a broken PDF is refused");
        assert_clean_rejection(&err);
        assert_eq!(
            err.to_string(),
            "extraction failed (extractor=inbound-pdf): the PDF could not be read",
            "the reason must be ours, not the parser's"
        );
    }

    /// A refused DOCX is refused with our own words too.
    #[cfg(feature = "kb-office")]
    #[tokio::test]
    async fn a_refused_docx_carries_our_own_words() {
        let err = extract_inbound_document(
            InboundDocumentKind::Docx,
            docx_without_its_document_rels(),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect_err("the DOCX is refused");
        assert_clean_rejection(&err);
        assert_eq!(
            err.to_string(),
            "extraction failed (extractor=inbound-docx): the DOCX document could not be read",
            "the reason must be ours, not the parser's"
        );
    }

    /// A reader that sends more than the frame allows is refused at once.
    ///
    /// The reader is still running and still writing when the cap is passed,
    /// so the parent must not wait for it: waiting fills the pipe and waits
    /// for the deadline instead of refusing the answer.
    #[tokio::test]
    async fn a_reader_that_sends_too_much_is_refused_at_once() {
        let started = std::time::Instant::now();
        let err = extract_test_document(
            "flood",
            &[],
            Vec::new(),
            limits(50, 100, Duration::from_secs(3)),
        )
        .await
        .expect_err("the flood is refused");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "the refusal must come at once, not at the deadline: {elapsed:?}"
        );
        assert!(
            err.to_string().contains("more than the allowed size"),
            "{err}"
        );
    }

    /// A reader that never returns is killed at the deadline, and the next
    /// document is read.
    #[tokio::test]
    async fn a_reader_that_sleeps_past_the_deadline_is_killed_and_the_next_document_is_read() {
        let started = std::time::Instant::now();
        let err = extract_test_document(
            "sleep",
            &[(SLEEP_SECS_VAR, "30")],
            Vec::new(),
            limits(50, 50_000, Duration::from_secs(1)),
        )
        .await
        .expect_err("the sleeping reader is killed");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "the deadline must fire, not the sleep: {elapsed:?}"
        );
        assert!(err.to_string().contains("too long"), "{err}");

        // The slot is free again: the next document is read.
        let extraction = extract_inbound_document(
            InboundDocumentKind::Text,
            b"after the deadline".to_vec(),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect("the next document is read");
        assert_eq!(extraction.text, "after the deadline");
    }

    /// An allocation past the ceiling is a rejection, not an agent that runs
    /// out of memory.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_allocation_past_the_ceiling_is_a_rejection() {
        let err = extract_test_document(
            "alloc",
            &[],
            Vec::new(),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect_err("the allocation past the ceiling is refused");
        assert_clean_rejection(&err);

        // The agent is still running.
        let extraction = extract_inbound_document(
            InboundDocumentKind::Text,
            b"still alive".to_vec(),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect("the agent is still alive");
        assert_eq!(extraction.text, "still alive");
    }

    /// The reader sees no environment: not the operator's home, not a value
    /// the parent holds.
    #[tokio::test]
    async fn the_reader_sees_no_environment() {
        // A value the parent holds and the child must not see.
        std::env::set_var(PROBE_VAR, "secret-marker");
        let extraction = extract_test_document(
            "env",
            &[],
            Vec::new(),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect("the reader answers");
        std::env::remove_var(PROBE_VAR);
        assert!(
            extraction.text.contains("probe=<absent>"),
            "{}",
            extraction.text
        );
        assert!(
            extraction.text.contains("home=<absent>"),
            "{}",
            extraction.text
        );
    }

    /// The reader is a process of its own.
    #[tokio::test]
    async fn the_reader_is_another_process() {
        let extraction = extract_test_document(
            "pid",
            &[],
            Vec::new(),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect("the reader answers");
        let reader: u32 = extraction.text.trim().parse().expect("a process id");
        assert_ne!(reader, std::process::id());
    }

    /// A kind the parent does not know is refused before a child is started.
    #[tokio::test]
    async fn an_unknown_kind_is_refused() {
        let err = extract_test_document(
            "nonsense",
            &[],
            Vec::new(),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect_err("an unknown kind is refused");
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    // ── The office reader ──────────────────────────────────────────────────

    /// A DOCX whose deflate stream is corrupt is a rejection, not a panic
    /// that takes the agent with it.
    #[cfg(feature = "kb-office")]
    #[tokio::test]
    async fn a_corrupt_deflate_docx_is_refused() {
        let err = extract_inbound_document(
            InboundDocumentKind::Docx,
            corrupt_deflate_docx(),
            limits(50, 50_000, Duration::from_secs(20)),
        )
        .await
        .expect_err("the corrupt DOCX is refused");
        assert_clean_rejection(&err);
    }

    /// The fixture really is hostile: `docx-rs` panics on it in process.
    #[cfg(feature = "kb-office")]
    #[test]
    #[should_panic]
    fn the_corrupt_docx_fixture_panics_the_parser() {
        let _ = docx_rs::read_docx(&corrupt_deflate_docx());
    }

    /// A DOCX with a corrupt deflate stream: the archive is a real ZIP, but
    /// the document part claims to be compressed and carries bytes that
    /// cannot be inflated.
    #[cfg(feature = "kb-office")]
    fn corrupt_deflate_docx() -> Vec<u8> {
        const CONTENT_TYPES: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
            r#"<Default Extension="xml" ContentType="application/xml"/>"#,
            r#"<Override ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml" PartName="/word/document.xml"/>"#,
            r#"</Types>"#
        );
        const PACKAGE_RELS: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>"#,
            r#"</Relationships>"#
        );
        const DOCUMENT_RELS: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"/>"#
        );
        // 0xff starts a deflate block with the reserved block type, so the
        // stream cannot be inflated.
        const BROKEN_DEFLATE: &[u8] = &[0xff, 0xff, 0xff, 0xff, 0xff];

        build_zip(&[
            ("[Content_Types].xml", true, CONTENT_TYPES.as_bytes(), 0),
            ("_rels/.rels", true, PACKAGE_RELS.as_bytes(), 0),
            (
                "word/_rels/document.xml.rels",
                true,
                DOCUMENT_RELS.as_bytes(),
                0,
            ),
            ("word/document.xml", false, BROKEN_DEFLATE, 100),
        ])
    }

    /// A DOCX whose package does not carry the document's own rels.
    ///
    /// The office processor refuses it, and its reason would carry the
    /// reader's own words — the reason the sender sees is ours.
    #[cfg(feature = "kb-office")]
    fn docx_without_its_document_rels() -> Vec<u8> {
        const CONTENT_TYPES: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
            r#"<Default Extension="xml" ContentType="application/xml"/>"#,
            r#"<Override ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml" PartName="/word/document.xml"/>"#,
            r#"</Types>"#
        );
        const PACKAGE_RELS: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
            r#"<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>"#,
            r#"</Relationships>"#
        );
        const DOCUMENT: &str = concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:body><w:p><w:r><w:t>hello</w:t></w:r></w:p></w:body></w:document>"#
        );
        build_zip(&[
            ("[Content_Types].xml", true, CONTENT_TYPES.as_bytes(), 0),
            ("_rels/.rels", true, PACKAGE_RELS.as_bytes(), 0),
            ("word/document.xml", true, DOCUMENT.as_bytes(), 0),
        ])
    }

    /// A ZIP archive with the given entries.
    ///
    /// `stored` says whether the entry is stored as it is; an entry that is
    /// not stored claims to be deflated and carries the bytes given.
    #[cfg(feature = "kb-office")]
    fn build_zip(entries: &[(&str, bool, &[u8], u32)]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut directory: Vec<u8> = Vec::new();
        for (name, stored, data, uncompressed_size) in entries {
            let offset = u32::try_from(out.len()).unwrap_or(0);
            let method: u16 = if *stored { 0 } else { 8 };
            let crc = if *stored { crc32(data) } else { 0 };
            let compressed = u32::try_from(data.len()).unwrap_or(0);
            let name_len = u16::try_from(name.len()).unwrap_or(0);

            out.extend_from_slice(&0x0403_4b50_u32.to_le_bytes());
            out.extend_from_slice(&20_u16.to_le_bytes());
            out.extend_from_slice(&0_u16.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0_u16.to_le_bytes());
            out.extend_from_slice(&0_u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&compressed.to_le_bytes());
            out.extend_from_slice(&uncompressed_size.to_le_bytes());
            out.extend_from_slice(&name_len.to_le_bytes());
            out.extend_from_slice(&0_u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);

            directory.extend_from_slice(&0x0201_4b50_u32.to_le_bytes());
            directory.extend_from_slice(&20_u16.to_le_bytes());
            directory.extend_from_slice(&20_u16.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&method.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&crc.to_le_bytes());
            directory.extend_from_slice(&compressed.to_le_bytes());
            directory.extend_from_slice(&uncompressed_size.to_le_bytes());
            directory.extend_from_slice(&name_len.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&0_u16.to_le_bytes());
            directory.extend_from_slice(&0_u32.to_le_bytes());
            directory.extend_from_slice(&offset.to_le_bytes());
            directory.extend_from_slice(name.as_bytes());
        }

        let count = u16::try_from(entries.len()).unwrap_or(0);
        let directory_offset = u32::try_from(out.len()).unwrap_or(0);
        let directory_size = u32::try_from(directory.len()).unwrap_or(0);
        out.extend_from_slice(&directory);
        out.extend_from_slice(&0x0605_4b50_u32.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&directory_size.to_le_bytes());
        out.extend_from_slice(&directory_offset.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out
    }

    /// The CRC-32 of a stored entry.
    #[cfg(feature = "kb-office")]
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFF_u32;
        for byte in data {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = if crc & 1 == 0 { 0 } else { 0xEDB8_8320 };
                crc = (crc >> 1) ^ mask;
            }
        }
        !crc
    }
}
