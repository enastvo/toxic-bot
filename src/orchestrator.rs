//! Orchestration layer: one lightweight actor per active room, fed by a
//! [`Dispatcher`], that coalesces a burst of near-simultaneous messages into a
//! single turn, dedupes exact-duplicate timestamps, and serializes all
//! generation across rooms behind a single global inference permit.
//!
//! ## Model (design rulings T11-a/b/c)
//! - **Per-room actors coalesce independently.** Each room has its own tokio
//!   task and mpsc channel. A burst of messages that arrive close together is
//!   drained into ONE call to [`TurnHandler::handle_burst`].
//! - **A single global [`Semaphore`] permit serializes generation.** The actor
//!   acquires the permit *around* `handle_burst` so only one turn generates at a
//!   time across every room. `handle_burst` itself must NOT acquire the permit;
//!   Task 22's summarizer acquires this same permit separately
//!   ([`Dispatcher::inference_permit`], ruling T11-a).
//! - **`TurnHandler` is the seam** so tests can inject a fake (ruling T11-c).
//!   [`Router`] implements it by forwarding to its inherent `handle_burst`.
//!
//! ## Idle reaping and the reap/dispatch race
//! An actor that sees no traffic for `idle` removes itself from the rooms map
//! and exits, to be respawned on the next dispatch. This introduces a race: a
//! `dispatch` may hold a `Sender` clone for an actor that is exiting. It is
//! closed on two sides so that no message is ever lost or acknowledged-but-
//! unprocessed:
//! - The **actor**, on idle timeout, takes the rooms lock, removes its own map
//!   entry (guarded by `same_channel` so a respawn is never clobbered), and
//!   calls `rx.close()`. `close()` is atomic with respect to `send`: any racing
//!   `send` either already landed in the buffer or now fails. It then releases
//!   the lock and DRAINS the buffer, processing any stragglers as one last
//!   coalesced turn before exiting — so a message that landed just before
//!   `close()` is still handled.
//! - The **dispatcher**, if a `send` fails (the channel is closed / the receiver
//!   is gone), reclaims the returned message, removes the stale entry (again
//!   guarded by `same_channel`), and retries — spawning a fresh actor and
//!   re-delivering. A failed send returns the message, so it is never dropped.

use crate::router::Router;
use crate::types::IncomingMessage;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::Semaphore;

/// Default idle window after which a quiet room's actor reaps itself (~30 min).
const DEFAULT_IDLE: Duration = Duration::from_secs(30 * 60);

/// Per-room channel capacity. Comfortably larger than any realistic burst so
/// that `dispatch` never blocks on a full channel in practice.
const CHANNEL_CAPACITY: usize = 1024;

/// The burst-processing seam (ruling T11-c). Implemented by [`Router`] in prod
/// and by a counting fake in tests.
#[async_trait::async_trait]
pub trait TurnHandler: Send + Sync {
    async fn handle_burst(
        &self,
        msgs: Vec<IncomingMessage>,
        wait_ms: u64,
    ) -> anyhow::Result<Option<String>>;
}

#[async_trait::async_trait]
impl TurnHandler for Router {
    async fn handle_burst(
        &self,
        msgs: Vec<IncomingMessage>,
        wait_ms: u64,
    ) -> anyhow::Result<Option<String>> {
        // Forward to the inherent method (the Task 1-10 implementation).
        Router::handle_burst(self, msgs, wait_ms).await
    }
}

/// A single in-flight generation: which room, and how long it has been
/// running (for Task 17's health view).
#[derive(Debug, Clone, Serialize)]
pub struct InFlight {
    pub room: String,
    pub elapsed_ms: u64,
}

/// Point-in-time orchestration gauges, read by Task 17's `/api/metrics`.
#[derive(Debug, Clone, Serialize, Default)]
pub struct DispatcherGauges {
    pub in_flight: Option<InFlight>,
    /// Best-effort per-room buffered-message counts. Left empty in v1: tokio's
    /// `mpsc::Sender`/`Receiver` expose no way to observe the current number
    /// of buffered items, so there is nothing cheap/accurate to report here.
    pub per_room_buffered: Vec<(String, usize)>,
    pub active_actors: usize,
}

/// Routes incoming messages to per-room actors and owns the single global
/// inference permit.
pub struct Dispatcher {
    rooms: Mutex<HashMap<String, mpsc::Sender<IncomingMessage>>>,
    handler: Arc<dyn TurnHandler>,
    permit: Arc<Semaphore>,
    idle: Duration,
    /// Marker for the room+start-time of the generation currently holding the
    /// global permit, if any. Shared with every `RoomActor` so `gauges()` can
    /// report it without touching the permit itself. Set (lock/write/drop)
    /// immediately after acquiring the permit and cleared (lock/write/drop)
    /// right after `handle_burst` returns — never held across an `.await`.
    in_flight: Arc<Mutex<Option<(String, Instant)>>>,
}

impl Dispatcher {
    /// Production constructor: wraps a [`Router`] as the handler, with the
    /// default idle window and a fresh global permit (`Semaphore::new(1)`).
    pub fn new(router: Arc<Router>) -> Arc<Dispatcher> {
        Self::with_handler(router, DEFAULT_IDLE)
    }

    /// Test/flexible constructor: inject any [`TurnHandler`] and idle window.
    pub fn with_handler(handler: Arc<dyn TurnHandler>, idle: Duration) -> Arc<Dispatcher> {
        Arc::new(Dispatcher {
            rooms: Mutex::new(HashMap::new()),
            handler,
            permit: Arc::new(Semaphore::new(1)),
            idle,
            in_flight: Arc::new(Mutex::new(None)),
        })
    }

    /// The single global inference permit (ruling T11-a). Task 22's summarizer
    /// shares this exact `Semaphore` so summarization and generation never run
    /// concurrently.
    pub fn inference_permit(&self) -> Arc<Semaphore> {
        self.permit.clone()
    }

    /// Number of rooms with a currently-live actor. Primarily for tests/metrics.
    pub fn active_rooms(&self) -> usize {
        self.rooms.lock().unwrap().len()
    }

    /// Point-in-time orchestration gauges for Task 17's health view.
    pub fn gauges(&self) -> DispatcherGauges {
        let active_actors = self.rooms.lock().unwrap().len();
        let in_flight = self
            .in_flight
            .lock()
            .unwrap()
            .as_ref()
            .map(|(room, since)| InFlight { room: room.clone(), elapsed_ms: since.elapsed().as_millis() as u64 });
        DispatcherGauges { in_flight, per_room_buffered: vec![], active_actors }
    }

    /// Deliver `msg` to its room's actor, spawning one if needed. If the actor
    /// exited between lookup and send (idle-reaped), the stale entry is dropped
    /// and a fresh actor is spawned so the message is never lost.
    pub async fn dispatch(self: &Arc<Self>, msg: IncomingMessage) {
        let room_id = msg.room_id.clone();
        let mut pending = msg;
        loop {
            // Look up (or create) the room's sender. The std Mutex guard is
            // released before the `.await` below — never held across an await.
            let sender = {
                let mut rooms = self.rooms.lock().unwrap();
                match rooms.get(&room_id) {
                    Some(tx) => tx.clone(),
                    None => {
                        let tx = self.spawn_actor(room_id.clone());
                        rooms.insert(room_id.clone(), tx.clone());
                        tx
                    }
                }
            };

            match sender.send(pending).await {
                Ok(()) => return,
                Err(mpsc::error::SendError(returned)) => {
                    // The actor's receiver is gone (idle-reaped). Reclaim the
                    // message, drop the stale map entry if it is still ours, and
                    // loop to respawn a fresh actor.
                    pending = returned;
                    let mut rooms = self.rooms.lock().unwrap();
                    if let Some(tx) = rooms.get(&room_id) {
                        if tx.same_channel(&sender) {
                            rooms.remove(&room_id);
                        }
                    }
                    // fall through: next iteration spawns fresh and resends.
                }
            }
        }
    }

    /// Spawn a fresh actor task for `room_id` and return the `Sender` to store.
    fn spawn_actor(self: &Arc<Self>, room_id: String) -> mpsc::Sender<IncomingMessage> {
        let (tx, rx) = mpsc::channel::<IncomingMessage>(CHANNEL_CAPACITY);
        let actor = RoomActor {
            room_id,
            rx,
            self_tx: tx.clone(),
            handler: self.handler.clone(),
            permit: self.permit.clone(),
            idle: self.idle,
            dispatcher: Arc::downgrade(self),
            last_processed_ts: None,
            in_flight: self.in_flight.clone(),
        };
        tokio::spawn(actor.run());
        tx
    }
}

/// One tokio task per active room.
struct RoomActor {
    room_id: String,
    rx: mpsc::Receiver<IncomingMessage>,
    /// A clone of our own sender, kept only to compare channel identity
    /// (`same_channel`) when reaping so we never remove a respawned actor's
    /// entry.
    self_tx: mpsc::Sender<IncomingMessage>,
    handler: Arc<dyn TurnHandler>,
    permit: Arc<Semaphore>,
    idle: Duration,
    /// Weak so live actor tasks don't keep the `Dispatcher` alive on their own.
    dispatcher: Weak<Dispatcher>,
    /// Max timestamp already handled; anything at or below it is a duplicate.
    last_processed_ts: Option<i64>,
    /// Shared with the [`Dispatcher`] (see its field doc); set around
    /// `handle_burst` so `gauges()` can report the in-flight room + elapsed.
    in_flight: Arc<Mutex<Option<(String, Instant)>>>,
}

impl RoomActor {
    async fn run(mut self) {
        loop {
            // Wait for the first message of a batch, or reap on idle. `biased`
            // makes us always prefer draining the queue over timing out.
            let head: Option<IncomingMessage> = tokio::select! {
                biased;
                m = self.rx.recv() => m, // Some(m) normal; None = channel closed & drained
                _ = tokio::time::sleep(self.idle) => {
                    // Idle fired: reap ourselves. This is the reap/dispatch race
                    // window — a `dispatch` may already hold a `Sender` clone and
                    // be about to `send().await`. We close that window with no
                    // lost or silently-dropped message:
                    //
                    //   1. Under the rooms lock, remove our map entry (guarded by
                    //      `same_channel` so a respawn is never clobbered) and
                    //      call `rx.close()`. `close()` is atomic w.r.t. `send`:
                    //      any racing send either already landed in the buffer
                    //      (we drain it below) or now fails with `SendError`
                    //      (the dispatcher's reclaim/respawn re-delivers it).
                    //   2. Release the lock, then DRAIN the buffer and process
                    //      any stragglers as one final coalesced turn before
                    //      exiting. The drain/generation awaits, so it must run
                    //      AFTER releasing the std MutexGuard.
                    let Some(disp) = self.dispatcher.upgrade() else { return; };
                    {
                        let mut rooms = disp.rooms.lock().unwrap();
                        if let Some(tx) = rooms.get(&self.room_id) {
                            if tx.same_channel(&self.self_tx) {
                                rooms.remove(&self.room_id);
                            }
                        }
                        self.rx.close();
                    } // rooms MutexGuard released here — never held across the await below.
                    self.process_from(None).await;
                    return;
                }
            };

            match head {
                Some(m) => self.process_from(Some(m)).await,
                // Channel closed and fully drained: nothing more can arrive.
                None => return,
            }
        }
    }

    /// Coalesce a batch starting from `head` (if any), draining everything else
    /// immediately available, dedupe, and — if the batch is non-empty — run it
    /// through the global inference permit. Updates `last_processed_ts`.
    ///
    /// Used both for the normal per-message path (`head = Some`) and for the
    /// reap drain (`head = None`, process whatever the closed channel still
    /// buffers).
    async fn process_from(&mut self, head: Option<IncomingMessage>) {
        // Record the batch's start instant. wait_ms = elapsed from pulling the
        // first message to just before generation, i.e. how long the burst was
        // allowed to coalesce. We deliberately measure it BEFORE acquiring the
        // permit so it reflects coalescing latency, not time spent queued behind
        // another room's generation. (Documented simplification per the brief:
        // one Instant for the batch head rather than per-message arrival stamps.)
        let batch_start = Instant::now();

        let mut batch: Vec<IncomingMessage> = Vec::new();
        let mut seen_ts: HashSet<i64> = HashSet::new();
        if let Some(h) = head {
            self.push_dedup(h, &mut batch, &mut seen_ts);
        }

        // Coalesce: drain everything immediately available without blocking.
        // (After `close()` this drains the remaining buffer, then stops.)
        while let Ok(m) = self.rx.try_recv() {
            self.push_dedup(m, &mut batch, &mut seen_ts);
        }

        if batch.is_empty() {
            // Everything in this wake-up was a duplicate (or nothing buffered).
            return;
        }

        let max_ts = batch.iter().map(|m| m.timestamp).max().unwrap();
        let wait_ms = batch_start.elapsed().as_millis() as u64;

        // Acquire the global permit AROUND the whole handle_burst call (ruling
        // T11-b). Messages arriving while we hold it queue in the mpsc and are
        // drained on the next loop iteration (coalesced).
        match self.permit.acquire().await {
            Ok(_permit) => {
                // Mark in-flight immediately after acquiring the permit and
                // before awaiting handle_burst; clear it right after, in both
                // outcomes. Lock/set/drop — never held across the await.
                {
                    let mut inf = self.in_flight.lock().unwrap();
                    *inf = Some((self.room_id.clone(), Instant::now()));
                }
                let result = self.handler.handle_burst(batch, wait_ms).await;
                {
                    let mut inf = self.in_flight.lock().unwrap();
                    *inf = None;
                }
                if let Err(e) = result {
                    // Log, don't crash the actor.
                    tracing::error!(room = %self.room_id, error = %e, "handle_burst failed");
                }
                // _permit dropped here -> released.
            }
            Err(_) => {
                // Semaphore closed (never in practice). Drop the batch.
                tracing::error!(room = %self.room_id, "inference permit closed");
            }
        }

        // Mark progress so exact-duplicate (and older) timestamps that arrive
        // later are dropped.
        self.last_processed_ts = Some(match self.last_processed_ts {
            Some(prev) => prev.max(max_ts),
            None => max_ts,
        });
    }

    /// Push `m` into `batch` unless its timestamp duplicates one already in the
    /// batch, or is at/below the last processed timestamp (already handled).
    ///
    /// Dedupe key is the (timestamp) ALONE, by design: the Signal server
    /// timestamp is a millisecond epoch and a true resend of a message carries
    /// the same timestamp, so an exact-ts match is the resend signal. Two
    /// genuinely distinct messages that happen to share a ts are intentionally
    /// treated as duplicates (acceptable — collisions at ms granularity in one
    /// room are vanishingly rare).
    fn push_dedup(
        &self,
        m: IncomingMessage,
        batch: &mut Vec<IncomingMessage>,
        seen_ts: &mut HashSet<i64>,
    ) {
        if let Some(lp) = self.last_processed_ts {
            if m.timestamp <= lp {
                return; // already processed in an earlier batch.
            }
        }
        if seen_ts.insert(m.timestamp) {
            batch.push(m);
        }
        // else: exact-duplicate timestamp within this batch -> drop.
    }
}
