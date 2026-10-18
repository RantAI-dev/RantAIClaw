use super::traits::{Channel, ChannelMessage, SendMessage};

/// Owner identity for the local console, qualified by its surface.
pub const CLI_SENDER_ID: &str = "cli:local";

/// The unqualified identity the console used before `cli:local`. Still accepted
/// as an alias so an existing `approval_owners = ["user"]` keeps working, but it
/// collides with any remote sender who picks that name.
pub const LEGACY_CLI_SENDER_ID: &str = "user";

use async_trait::async_trait;
use tokio::io::{self, AsyncBufReadExt, BufReader};
use uuid::Uuid;

/// CLI channel — stdin/stdout, always available, zero deps
pub struct CliChannel;

impl CliChannel {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Channel for CliChannel {
    fn name(&self) -> &str {
        "cli"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        // Strip markup to readable text: a terminal renders no markdown, so
        // `**bold**`/tables would show as literal syntax.
        let rendered = crate::channels::format::render_to_string(
            &message.content,
            &crate::channels::format::RenderTarget::Plain,
        );
        println!("{rendered}");
        Ok(())
    }

    async fn listen(
        &self,
        tx: tokio::sync::mpsc::Sender<ChannelMessage>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let stdin = io::stdin();
        let reader = BufReader::new(stdin);
        let lines = reader.lines();
        // Loop body lives in a helper so the cancel arm can be exercised in a
        // test against a reader that never yields (under `cargo test` stdin
        // can be `/dev/null`, which ends the read loop at EOF and would let a
        // missing cancel arm pass the test by returning early).
        run_stdin_loop(tx, cancel, lines).await
    }
}

/// Read lines from `lines` until cancellation or EOF, forwarding each
/// non-empty, non-`/quit` line to `tx`. Cancellation wins the tie against
/// the next line so shutdown completes inside the supervisor's grace and is
/// not dropped mid-read.
async fn run_stdin_loop<R>(
    tx: tokio::sync::mpsc::Sender<ChannelMessage>,
    cancel: tokio_util::sync::CancellationToken,
    mut lines: tokio::io::Lines<R>,
) -> anyhow::Result<()>
where
    R: io::AsyncBufRead + Unpin,
{
    loop {
        let line = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            line = lines.next_line() => match line {
                Ok(Some(line)) => line,
                // EOF on stdin or an I/O error both end the loop cleanly:
                // the channel is gone, no platform teardown is owed.
                Ok(None) | Err(_) => return Ok(()),
            },
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "/quit" || line == "/exit" {
            return Ok(());
        }

        let msg = ChannelMessage {
            id: Uuid::new_v4().to_string(),
            // Channel-qualified so a remote sender cannot pick this name.
            // It used to be the bare string "user", compared against
            // `approval_owners` with no channel qualification — so an
            // operator who followed the hint and added "user" also made a
            // Telegram account renamed to `user`, or an IRC nick `user`, an
            // owner. `sender_aliases` keeps existing `["user"]` configs
            // working; `warn_on_legacy_cli_owner` tells them to migrate.
            sender: CLI_SENDER_ID.to_string(),
            sender_aliases: vec![LEGACY_CLI_SENDER_ID.to_string()],
            reply_target: CLI_SENDER_ID.to_string(),
            content: line,
            channel: "cli".to_string(),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            thread_ts: None,
            reply_anchor: None,
            // The local terminal is a DM by construction: only the
            // operator can type into it, and no one else can read it.
            is_direct: true,
        };

        if tx.send(msg).await.is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_channel_name() {
        assert_eq!(CliChannel::new().name(), "cli");
    }

    #[test]
    fn plain_render_strips_markdown() {
        // Contract lock for the whole Plain-baseline group (signal/qq/linq/lark/
        // irc/imessage/nextcloud/email/cli): their send() routes content through
        // this, so if plain.rs stopped stripping markup the wiring would silently
        // ship `**bold**` again. Headings uppercase, `**bold**` -> `bold`.
        let out = crate::channels::format::render_to_string(
            "## Hi\n\n**bold**",
            &crate::channels::format::RenderTarget::Plain,
        );
        assert_eq!(out, "HI\n\nbold");
    }

    #[tokio::test]
    async fn cli_channel_send_does_not_panic() {
        let ch = CliChannel::new();
        let result = ch
            .send(&SendMessage {
                content: "hello".into(),
                recipient: "user".into(),
                subject: None,
                thread_ts: None,
                reply_anchor: None,
                may_attach: false,
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cli_channel_send_empty_message() {
        let ch = CliChannel::new();
        let result = ch
            .send(&SendMessage {
                content: String::new(),
                recipient: String::new(),
                subject: None,
                thread_ts: None,
                reply_anchor: None,
                may_attach: false,
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cli_channel_health_check() {
        let ch = CliChannel::new();
        assert!(ch.health_check().await);
    }

    #[test]
    fn channel_message_struct() {
        let msg = ChannelMessage {
            sender_aliases: Vec::new(),
            id: "test-id".into(),
            sender: "user".into(),
            reply_target: "user".into(),
            content: "hello".into(),
            channel: "cli".into(),
            timestamp: 1_234_567_890,
            thread_ts: None,
            reply_anchor: None,
            is_direct: false,
        };
        assert_eq!(msg.id, "test-id");
        assert_eq!(msg.sender, "user");
        assert_eq!(msg.reply_target, "user");
        assert_eq!(msg.content, "hello");
        assert_eq!(msg.channel, "cli");
        assert_eq!(msg.timestamp, 1_234_567_890);
    }

    /// The stdin loop honours cancellation while blocked on a line. Under
    /// `cargo test` stdin may be `/dev/null`, which would end the loop at
    /// EOF regardless of the cancel arm — so the extracted function is driven
    /// directly against a duplex reader whose writer side stays open but
    /// never writes, standing in for a terminal that is not being typed on.
    #[tokio::test]
    async fn cancelling_stops_the_stdin_loop_at_its_wait_point() {
        let (writer, reader) = tokio::io::duplex(64);
        let lines = tokio::io::BufReader::new(reader).lines();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);

        let loop_handle = tokio::spawn(run_stdin_loop(tx, cancel.clone(), lines));

        // The loop must still be waiting: without a cancel arm it would
        // either finish at once (no source) or block forever here — in both
        // cases the assertions below prove the cancel arm is what fires.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !loop_handle.is_finished(),
            "the stdin loop must still be waiting on a line before cancellation fires"
        );

        cancel.cancel();
        let finished = tokio::time::timeout(std::time::Duration::from_secs(1), loop_handle).await;
        let loop_result = finished
            .expect("loop must return well inside the shutdown grace")
            .expect("loop task must not panic");
        assert!(
            loop_result.is_ok(),
            "cancellation must end the loop with Ok(()), not an error"
        );
        // Nothing was written, so nothing should have been forwarded.
        assert!(
            rx.try_recv().is_err(),
            "the loop must not emit a message when no line arrives"
        );
        // Keep the writer half alive for the whole test so the read side
        // never sees EOF and the "still waiting" assertion above is not an
        // artefact of a closed pipe.
        drop(writer);
    }

    #[test]
    fn channel_message_clone() {
        let msg = ChannelMessage {
            sender_aliases: Vec::new(),
            id: "id".into(),
            sender: "s".into(),
            reply_target: "s".into(),
            content: "c".into(),
            channel: "ch".into(),
            timestamp: 0,
            thread_ts: None,
            reply_anchor: None,
            is_direct: false,
        };
        let cloned = msg.clone();
        assert_eq!(cloned.id, msg.id);
        assert_eq!(cloned.content, msg.content);
    }
}
