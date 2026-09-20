use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const RING_CAP: usize = 500;
const ONE_HOUR_MS: i64 = 3_600_000;

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

/// A single recorded turn (one message processed by the bot).
#[derive(Debug, Clone)]
pub struct TurnRecord {
    pub room_id: String,
    pub ts: i64,
    pub decision: &'static str,
    pub wait_ms: u64,
    pub gen_ms: u64,
    pub prompt_tokens: u32,
    pub reply_tokens: u32,
    pub outcome: &'static str,
}

/// Fixed-capacity ring buffer of the most recent turn records.
struct Ring {
    buf: VecDeque<TurnRecord>,
}

impl Ring {
    fn new() -> Self {
        Ring { buf: VecDeque::with_capacity(RING_CAP) }
    }

    fn push(&mut self, r: TurnRecord) {
        if self.buf.len() >= RING_CAP {
            self.buf.pop_front();
        }
        self.buf.push_back(r);
    }
}

/// Aggregated view over the metrics ring, computed at snapshot time.
#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub replies: u64,
    pub errors: u64,
    pub timeouts: u64,
    pub avg_gen_ms: u64,
    pub p50_gen_ms: u64,
    pub p95_gen_ms: u64,
    pub avg_prompt_tokens: u32,
    pub avg_reply_tokens: u32,
    pub avg_tokens_per_sec: f32,
    /// Ring contents in chronological order (oldest first).
    pub recent: Vec<TurnRecord>,
}

/// In-memory store of recent turn records with response-time aggregates.
pub struct Metrics {
    inner: Mutex<Ring>,
}

impl Metrics {
    pub fn new() -> Arc<Metrics> {
        Arc::new(Metrics { inner: Mutex::new(Ring::new()) })
    }

    pub fn record(&self, r: TurnRecord) {
        let mut ring = self.inner.lock().unwrap();
        ring.push(r);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let ring = self.inner.lock().unwrap();
        let all: Vec<TurnRecord> = ring.buf.iter().cloned().collect();
        drop(ring);

        let replies = all.iter().filter(|r| r.outcome == "sent").count() as u64;
        let errors = all.iter().filter(|r| r.outcome == "error").count() as u64;
        let timeouts = all.iter().filter(|r| r.outcome == "timeout").count() as u64;

        let now = now_ms();
        let gen_records: Vec<&TurnRecord> = all
            .iter()
            .filter(|r| r.gen_ms > 0 && now - r.ts <= ONE_HOUR_MS)
            .collect();

        let (avg_gen_ms, p50_gen_ms, p95_gen_ms) = percentiles(&gen_records);

        let (avg_prompt_tokens, avg_reply_tokens, avg_tokens_per_sec) =
            token_averages(&gen_records);

        MetricsSnapshot {
            replies,
            errors,
            timeouts,
            avg_gen_ms,
            p50_gen_ms,
            p95_gen_ms,
            avg_prompt_tokens,
            avg_reply_tokens,
            avg_tokens_per_sec,
            recent: all,
        }
    }
}

/// Nearest-rank percentile helper: sorts a copy of `gen_ms` values and
/// returns (avg, p50, p95). Returns (0, 0, 0) when empty.
fn percentiles(records: &[&TurnRecord]) -> (u64, u64, u64) {
    if records.is_empty() {
        return (0, 0, 0);
    }
    let mut vals: Vec<u64> = records.iter().map(|r| r.gen_ms).collect();
    vals.sort_unstable();

    let sum: u64 = vals.iter().sum();
    let avg = sum / vals.len() as u64;

    let p50 = nearest_rank(&vals, 0.50);
    let p95 = nearest_rank(&vals, 0.95);

    (avg, p50, p95)
}

/// Nearest-rank percentile over an already-sorted slice.
fn nearest_rank(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((pct * sorted.len() as f64).ceil() as usize).max(1);
    let idx = rank.min(sorted.len()) - 1;
    sorted[idx]
}

fn token_averages(records: &[&TurnRecord]) -> (u32, u32, f32) {
    if records.is_empty() {
        return (0, 0, 0.0);
    }
    let n = records.len() as u32;
    let prompt_sum: u64 = records.iter().map(|r| r.prompt_tokens as u64).sum();
    let reply_sum: u64 = records.iter().map(|r| r.reply_tokens as u64).sum();

    let avg_prompt_tokens = (prompt_sum / n as u64) as u32;
    let avg_reply_tokens = (reply_sum / n as u64) as u32;

    let tps_sum: f32 = records
        .iter()
        .map(|r| {
            if r.gen_ms == 0 {
                0.0
            } else {
                r.reply_tokens as f32 / (r.gen_ms as f32 / 1000.0)
            }
        })
        .sum();
    let avg_tokens_per_sec = tps_sum / n as f32;

    (avg_prompt_tokens, avg_reply_tokens, avg_tokens_per_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(gen_ms: u64, prompt_tokens: u32, reply_tokens: u32, outcome: &'static str) -> TurnRecord {
        TurnRecord {
            room_id: "room1".to_string(),
            ts: now_ms(),
            decision: "reply",
            wait_ms: 0,
            gen_ms,
            prompt_tokens,
            reply_tokens,
            outcome,
        }
    }

    #[test]
    fn snapshot_computes_avg_p50_p95_over_gen_ms() {
        let m = Metrics::new();
        for gen_ms in [100u64, 200, 300, 400] {
            m.record(rec(gen_ms, 10, 5, "sent"));
        }
        let snap = m.snapshot();
        assert_eq!(snap.avg_gen_ms, 250);
        assert!(
            snap.p50_gen_ms == 200 || snap.p50_gen_ms == 300,
            "p50 was {}",
            snap.p50_gen_ms
        );
        assert_eq!(snap.p95_gen_ms, 400);
        assert_eq!(snap.replies, 4);
    }

    #[test]
    fn snapshot_counts_outcomes() {
        let m = Metrics::new();
        m.record(rec(100, 10, 5, "sent"));
        m.record(rec(100, 10, 5, "sent"));
        m.record(rec(0, 0, 0, "error"));
        m.record(rec(0, 0, 0, "timeout"));
        m.record(rec(0, 0, 0, "skipped"));
        let snap = m.snapshot();
        assert_eq!(snap.replies, 2);
        assert_eq!(snap.errors, 1);
        assert_eq!(snap.timeouts, 1);
    }

    #[test]
    fn snapshot_averages_tokens_and_tokens_per_sec_over_generation_records() {
        let m = Metrics::new();
        // 1000ms gen, 10 reply tokens -> 10 tok/s
        m.record(rec(1000, 20, 10, "sent"));
        // 500ms gen, 5 reply tokens -> 10 tok/s
        m.record(rec(500, 10, 5, "sent"));
        let snap = m.snapshot();
        assert_eq!(snap.avg_prompt_tokens, 15);
        assert_eq!(snap.avg_reply_tokens, 7); // (10+5)/2 = 7 (integer)
        assert!((snap.avg_tokens_per_sec - 10.0).abs() < 0.001);
    }

    #[test]
    fn snapshot_ignores_zero_gen_ms_records_for_gen_aggregates() {
        let m = Metrics::new();
        m.record(rec(0, 0, 0, "skipped"));
        m.record(rec(200, 10, 5, "sent"));
        let snap = m.snapshot();
        assert_eq!(snap.avg_gen_ms, 200);
        assert_eq!(snap.avg_prompt_tokens, 10);
    }

    #[test]
    fn snapshot_excludes_records_older_than_one_hour() {
        let m = Metrics::new();
        let old = TurnRecord {
            room_id: "room1".to_string(),
            ts: now_ms() - ONE_HOUR_MS - 1000,
            decision: "reply",
            wait_ms: 0,
            gen_ms: 999,
            prompt_tokens: 50,
            reply_tokens: 50,
            outcome: "sent",
        };
        m.record(old);
        m.record(rec(100, 10, 5, "sent"));
        let snap = m.snapshot();
        assert_eq!(snap.avg_gen_ms, 100);
        // but the stale record is still present in `recent` (raw ring, not filtered)
        assert_eq!(snap.recent.len(), 2);
    }

    #[test]
    fn ring_caps_at_500_dropping_oldest() {
        let m = Metrics::new();
        for i in 0..600u64 {
            m.record(rec(i, 0, 0, "sent"));
        }
        let snap = m.snapshot();
        assert_eq!(snap.recent.len(), 500);
        // oldest 100 (gen_ms 0..100) should have been dropped; first remaining is 100
        assert_eq!(snap.recent.first().unwrap().gen_ms, 100);
        assert_eq!(snap.recent.last().unwrap().gen_ms, 599);
    }
}
