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

/// Max messages fetched (and summarized) per room per sweep. Bounds both the
/// query and the transcript handed to the LLM. Fetches are oldest-unprocessed-first
/// (see [`crate::store::Store::messages_since`]), so a backlog larger than this cap
/// is never skipped — `covered_through_ts` only ever advances to the last message
/// actually fetched, so the room stays "due" and the next sweep picks up exactly
/// where this one left off, chunk by chunk, until it catches up.
///
/// Kept small (40) so the transcript stays well within the per-request timeout:
/// on the CPU host a 200-message transcript pushed summary generation past the
/// Ollama timeout and every sweep failed; ~40 messages summarizes in under a
/// minute. Larger backlogs are still covered incrementally over multiple sweeps.
const RECENT_CAP: i64 = 40;

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
    // Per-request timeout for summarization, read from live settings so a web-UI
    // edit applies without restart. Fall back to a sane default if settings are
    // unreadable — a bad read must not kill the sweep.
    let timeout_secs = store
        .get_settings()
        .await
        .map(|s| s.ollama_timeout_secs.max(1) as u64)
        .unwrap_or(300);
    for room in store.rooms_with_new_messages_since_summary().await? {
        let prior = store.get_summary(&room).await?;
        let covered_through_ts = prior.as_ref().map(|s| s.covered_through_ts).unwrap_or(0);
        let prior_summary = prior.map(|s| s.summary).unwrap_or_default();

        // Oldest-unprocessed-first: never skips over a backlog larger than RECENT_CAP.
        let new_msgs = store.messages_since(&room, covered_through_ts, RECENT_CAP).await?;
        if new_msgs.is_empty() {
            continue;
        }

        // Advance only to the last message actually fetched. If the backlog exceeded
        // RECENT_CAP, this is still < the room's true MAX(ts), so
        // `rooms_with_new_messages_since_summary` keeps surfacing it and the next
        // sweep fetches the next chunk — monotonic catch-up, no gap, no loss.
        let new_covered_through_ts = new_msgs.last().map(|m| m.ts).unwrap_or(covered_through_ts);
        let mut transcript = String::new();
        for m in &new_msgs {
            let who = m.sender_name.as_deref().unwrap_or(m.sender_id.as_str());
            transcript.push_str(&format!("{}: {}\n", who, m.body));
        }

        let new_summary = {
            // Accepted behavior: the global inference permit guarantees a summary
            // never overlaps live reply generation. A live reply can therefore
            // briefly wait behind an in-flight summary (bounded by the summary's
            // num_predict:300). True preemption of a summary by a live reply is a
            // known fast-follow, not implemented here.
            let _permit = permit.acquire().await?;
            llm.summarize(model, &prior_summary, &transcript, timeout_secs).await?
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
