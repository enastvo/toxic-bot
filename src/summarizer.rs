//! Per-room summarization sweep.
//!
//! Periodically folds new messages into a rolling per-room summary so that
//! long-running conversations stay within context without re-reading full
//! history on every turn. Activity-gated: a room is only re-summarized when
//! it has messages newer than its last `covered_through_ts` (see
//! [`crate::store::Store::rooms_with_new_messages_since_summary`]). Permit-guarded:
//! the LLM call shares the same global inference [`Semaphore`] as live reply
//! generation, so a sweep never competes with a user-facing generation for
//! the model — it simply waits its turn.

use crate::llm::LlmBackend;
use crate::store::Store;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

/// Cap on how many recent messages we ever load per room before filtering
/// down to those newer than the prior `covered_through_ts`. Bounds both the
/// query and the transcript handed to the LLM.
const RECENT_CAP: i64 = 200;

/// Run one summarization pass over every room with unsummarized activity.
///
/// Returns the number of rooms that were (re)summarized.
pub async fn run_sweep_once(
    store: &Store,
    llm: &Arc<dyn LlmBackend>,
    model: &str,
    permit: &Arc<Semaphore>,
) -> anyhow::Result<usize> {
    let mut count = 0usize;
    for room in store.rooms_with_new_messages_since_summary().await? {
        let prior = store.get_summary(&room).await?;
        let covered_through_ts = prior.as_ref().map(|s| s.covered_through_ts).unwrap_or(0);
        let prior_summary = prior.map(|s| s.summary).unwrap_or_default();

        let recent = store.recent(&room, RECENT_CAP).await?;
        let new_msgs: Vec<_> = recent.into_iter().filter(|m| m.ts > covered_through_ts).collect();
        if new_msgs.is_empty() {
            continue;
        }

        let new_covered_through_ts = new_msgs.iter().map(|m| m.ts).max().unwrap_or(covered_through_ts);
        let mut transcript = String::new();
        for m in &new_msgs {
            let who = m.sender_name.as_deref().unwrap_or(m.sender_id.as_str());
            transcript.push_str(&format!("{}: {}\n", who, m.body));
        }

        let new_summary = {
            let _permit = permit.acquire().await?;
            llm.summarize(model, &prior_summary, &transcript).await?
        };

        store.upsert_summary(&room, &new_summary, new_covered_through_ts).await?;
        count += 1;
    }
    Ok(count)
}

/// Spawn a background task that runs [`run_sweep_once`] on a fixed interval,
/// logging (but not propagating) errors so one bad sweep never kills the loop.
///
/// Whether summarization is enabled at all, and what interval to use, is the
/// caller's decision (settings-driven) — this function just loops.
pub fn spawn(store: Store, llm: Arc<dyn LlmBackend>, model: String, permit: Arc<Semaphore>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if let Err(e) = run_sweep_once(&store, &llm, &model, &permit).await {
                tracing::warn!(error = %e, "summarization sweep failed");
            }
        }
    });
}
