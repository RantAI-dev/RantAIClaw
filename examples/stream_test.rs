// End-to-end streaming test: goes through the same actor bridge the TUI uses,
// so we can catch any layer between provider and TUI that batches chunks.

use rantaiclaw::agent::agent::Agent;
use rantaiclaw::agent::events::AgentEvent;
use rantaiclaw::tui::{TuiAgentActor, TurnRequest};
use std::time::Instant;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = rantaiclaw::config::Config::load_or_init().await?;
    config.apply_env_overrides();
    let agent = Agent::from_config(&config).await?;

    let (req_tx, req_rx) = mpsc::channel::<TurnRequest>(16);
    let (events_tx, mut events_rx) = mpsc::channel::<AgentEvent>(128);

    let actor = TuiAgentActor::new(agent, req_rx, events_tx);
    let actor_handle = tokio::spawn(actor.run());

    let start = Instant::now();
    req_tx
        .send(TurnRequest::Submit(
            "write me a 3-sentence travel description of Bali".to_string(),
        ))
        .await?;

    let mut chunks_seen = 0;
    let mut first_chunk_at = None;
    while let Some(ev) = events_rx.recv().await {
        let elapsed = start.elapsed().as_millis();
        match ev {
            AgentEvent::Chunk(s) => {
                if first_chunk_at.is_none() {
                    first_chunk_at = Some(elapsed);
                }
                chunks_seen += 1;
                eprintln!("[{elapsed:>5}ms] Chunk({:?}) len={}", &s, s.len());
            }
            AgentEvent::Done {
                final_text,
                cancelled,
            } => {
                eprintln!(
                    "[{elapsed:>5}ms] Done cancelled={cancelled} text_len={}",
                    final_text.len()
                );
                break;
            }
            AgentEvent::Error(e) => {
                eprintln!("[{elapsed:>5}ms] Error: {e}");
                break;
            }
            other => eprintln!("[{elapsed:>5}ms] {other:?}"),
        }
    }
    eprintln!("\n=== {chunks_seen} chunks total, first at {first_chunk_at:?}ms ===");

    drop(req_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), actor_handle).await;
    Ok(())
}
