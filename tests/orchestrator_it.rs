//! Integration tests for the orchestrator: per-room actors, burst coalescing,
//! timestamp dedupe, and the global inference permit that serializes generation
//! across rooms.
//!
//! ## Runtime / flakiness reasoning
//! - `burst_coalesces` and `dedupe` use `flavor = "current_thread"`. On a
//!   single-threaded runtime a `tokio::spawn`ed actor does NOT run until the
//!   current task hits a real yield point. Because the mpsc channel has spare
//!   capacity, every `dispatch(..).await` completes without yielding, so all the
//!   messages are buffered *before* the actor is ever polled. The actor then
//!   drains them into ONE batch — making the coalescing/dedupe assertions exact
//!   and deterministic rather than timing-dependent.
//! - `two_rooms_independent`, `global_serialization` and `idle_reap_and_respawn`
//!   use the multi-thread runtime with generous timeouts and *poll* for a
//!   condition (see `wait_until`) instead of asserting exact wall-clock timings,
//!   so they exercise real cross-thread concurrency without being racy.

use async_trait::async_trait;
use signal_bot::orchestrator::{Dispatcher, TurnHandler};
use signal_bot::types::IncomingMessage;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A counting fake `TurnHandler`. Records how many times it was invoked, how
/// many messages it saw in total, the timestamps seen, and — crucially — a
/// max-concurrent-in-flight tracker to prove the global permit serializes
/// generation. Each call sleeps for a configurable duration to simulate the LLM
/// generation window during which further messages should coalesce.
struct FakeHandler {
    invocations: AtomicUsize,
    messages_seen: AtomicUsize,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    seen_ts: Mutex<Vec<i64>>,
    sleep: Duration,
}

impl FakeHandler {
    fn new(sleep: Duration) -> Self {
        Self {
            invocations: AtomicUsize::new(0),
            messages_seen: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            seen_ts: Mutex::new(Vec::new()),
            sleep,
        }
    }
}

#[async_trait]
impl TurnHandler for FakeHandler {
    async fn handle_burst(
        &self,
        msgs: Vec<IncomingMessage>,
        _wait_ms: u64,
    ) -> anyhow::Result<Option<String>> {
        self.invocations.fetch_add(1, SeqCst);
        self.messages_seen.fetch_add(msgs.len(), SeqCst);
        {
            let mut g = self.seen_ts.lock().unwrap();
            for m in &msgs {
                g.push(m.timestamp);
            }
        }
        // Track peak concurrency: increment on entry, record the max, sleep to
        // hold the "in flight" state, then decrement on exit.
        let cur = self.in_flight.fetch_add(1, SeqCst) + 1;
        self.max_in_flight.fetch_max(cur, SeqCst);
        tokio::time::sleep(self.sleep).await;
        self.in_flight.fetch_sub(1, SeqCst);
        Ok(Some("ok".to_string()))
    }
}

fn mk_msg(room: &str, ts: i64) -> IncomingMessage {
    IncomingMessage {
        room_id: room.into(),
        sender_id: "+1".into(),
        sender_name: Some("A".into()),
        body: format!("m{ts}"),
        is_group: false,
        is_mention: false,
        quoted_msg: None,
        timestamp: ts,
    }
}

/// Poll `cond` until it is true or `timeout` elapses. Returns the final value.
async fn wait_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cond()
}

/// Five messages dispatched to one room before the actor is polled must be
/// coalesced into a single turn that sees all five — never five single-message
/// turns.
#[tokio::test(flavor = "current_thread")]
async fn burst_coalesces() {
    let fake = Arc::new(FakeHandler::new(Duration::from_millis(50)));
    // Long idle so the actor never reaps mid-test.
    let disp = Dispatcher::with_handler(fake.clone(), Duration::from_secs(3600));

    for ts in 1..=5 {
        disp.dispatch(mk_msg("roomA", ts)).await;
    }
    // First yield point: lets the (single) actor task run and drain the buffer.
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        fake.invocations.load(SeqCst),
        1,
        "burst must coalesce into exactly one turn"
    );
    assert_eq!(
        fake.messages_seen.load(SeqCst),
        5,
        "the single turn must see all five distinct messages"
    );
    let mut ts = fake.seen_ts.lock().unwrap().clone();
    ts.sort_unstable();
    assert_eq!(ts, vec![1, 2, 3, 4, 5]);
}

/// The same message (identical timestamp) dispatched twice must be processed
/// exactly once.
#[tokio::test(flavor = "current_thread")]
async fn dedupe() {
    let fake = Arc::new(FakeHandler::new(Duration::from_millis(10)));
    let disp = Dispatcher::with_handler(fake.clone(), Duration::from_secs(3600));

    let m = mk_msg("roomA", 42);
    disp.dispatch(m.clone()).await;
    disp.dispatch(m.clone()).await;

    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        fake.messages_seen.load(SeqCst),
        1,
        "exact-duplicate timestamp must be dropped"
    );
    assert_eq!(fake.seen_ts.lock().unwrap().clone(), vec![42]);
}

/// Two rooms each receiving a message are both processed within a bounded time:
/// per-room actors are independent tasks, so B is not starved behind A. (We do
/// NOT assert that generation overlaps — the global permit serializes that.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_rooms_independent() {
    let fake = Arc::new(FakeHandler::new(Duration::from_millis(30)));
    let disp = Dispatcher::with_handler(fake.clone(), Duration::from_secs(3600));

    disp.dispatch(mk_msg("roomA", 1)).await;
    disp.dispatch(mk_msg("roomB", 1)).await;

    // Serialized worst case is ~2*30ms; 1s is a generous, non-racy bound.
    let done = wait_until(
        || fake.invocations.load(SeqCst) >= 2,
        Duration::from_millis(1000),
    )
    .await;
    assert!(done, "both rooms must make progress within a bounded time");
    assert_eq!(
        fake.messages_seen.load(SeqCst),
        2,
        "each room's message must be processed"
    );
}

/// Bursts to several rooms with a sleeping fake: the global permit must ensure
/// no two generations are ever in flight at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_serialization() {
    let fake = Arc::new(FakeHandler::new(Duration::from_millis(40)));
    let disp = Dispatcher::with_handler(fake.clone(), Duration::from_secs(3600));

    for r in 0..4 {
        let room = format!("room{r}");
        for ts in 1..=3 {
            disp.dispatch(mk_msg(&room, ts)).await;
        }
    }

    // Up to ~8 serialized batches * 40ms; poll up to 2s.
    // Wait until ALL dispatched work has fully COMPLETED, not merely started:
    // every distinct message (4 rooms * 3 distinct ts = 12) has been seen AND
    // nothing is still in flight. Only then can we be sure a late overlap would
    // have been observed by `max_in_flight`.
    let done = wait_until(
        || fake.messages_seen.load(SeqCst) == 12 && fake.in_flight.load(SeqCst) == 0,
        Duration::from_millis(2000),
    )
    .await;
    assert!(done, "all dispatched work must complete");
    assert!(
        fake.invocations.load(SeqCst) >= 4,
        "every room must be processed at least once"
    );
    assert_eq!(
        fake.max_in_flight.load(SeqCst),
        1,
        "the global permit must serialize all generations across rooms"
    );
}

/// An idle actor reaps itself; a later dispatch respawns a fresh actor and the
/// message is still processed (exercises the reap/dispatch handling).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_reap_and_respawn() {
    let fake = Arc::new(FakeHandler::new(Duration::from_millis(5)));
    // Short idle so the actor reaps quickly after going quiet.
    let disp = Dispatcher::with_handler(fake.clone(), Duration::from_millis(100));

    disp.dispatch(mk_msg("roomA", 1)).await;
    assert!(
        wait_until(|| fake.invocations.load(SeqCst) == 1, Duration::from_millis(500)).await,
        "first message processed"
    );

    // Wait comfortably past the idle window so the actor reaps itself.
    assert!(
        wait_until(|| disp.active_rooms() == 0, Duration::from_millis(1000)).await,
        "idle actor must reap itself from the rooms map"
    );

    // A fresh dispatch must respawn an actor and get processed.
    disp.dispatch(mk_msg("roomA", 2)).await;
    assert!(
        wait_until(|| fake.invocations.load(SeqCst) == 2, Duration::from_millis(500)).await,
        "respawned actor must process the new message"
    );
}

/// Hammer the reap boundary: dispatch many distinct messages with inter-arrival
/// pauses that straddle a very short idle window, so dispatches repeatedly race
/// actor reaps. Every message must still be processed exactly once — the
/// drain-on-close reap path plus the dispatcher's send-failure reclaim guarantee
/// no message is lost or silently dropped across a reap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_message_lost_across_reap() {
    let fake = Arc::new(FakeHandler::new(Duration::from_millis(2)));
    // Idle short enough that the actor reaps between many of the dispatches.
    let disp = Dispatcher::with_handler(fake.clone(), Duration::from_millis(15));

    const N: i64 = 40;
    for ts in 1..=N {
        disp.dispatch(mk_msg("roomA", ts)).await;
        // Pauses that hover around the idle window to maximize reap/dispatch
        // interleaving: alternate under- and over-idle sleeps.
        let nap = if ts % 2 == 0 { 8 } else { 20 };
        tokio::time::sleep(Duration::from_millis(nap)).await;
    }

    // All N distinct messages must be seen (coalescing may bundle some into the
    // same turn, so we assert on total messages, not invocation count).
    let done = wait_until(
        || fake.messages_seen.load(SeqCst) == N as usize,
        Duration::from_millis(2000),
    )
    .await;
    let seen = fake.messages_seen.load(SeqCst);
    assert!(done, "no message may be lost across reaps: saw {seen}/{N}");

    let mut ts = fake.seen_ts.lock().unwrap().clone();
    ts.sort_unstable();
    assert_eq!(ts, (1..=N).collect::<Vec<_>>(), "every ts processed once");
}
