use signal_bot::llm::{LlmBackend, MockLlm, Relevance};
use signal_bot::store::{NewMessage, Store};
use signal_bot::summarizer::run_sweep_once;
use signal_bot::types::Role;
use std::sync::Arc;
use tokio::sync::Semaphore;

fn mock_llm() -> Arc<dyn LlmBackend> {
    Arc::new(MockLlm { reply: "unused".into(), relevance: Relevance { should_reply: false, confidence: 0.0 } })
}

#[tokio::test]
async fn sweep_summarizes_active_room_then_gates_until_new_activity() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.ensure_room("G", Some("Group"), true).await.unwrap();

    store.record_message(NewMessage {
        room_id: "G".into(), sender_id: "alice".into(), sender_name: Some("Alice".into()),
        role: Role::User, body: "hello".into(), ts: 100, personality: None, is_mention: false,
    }).await.unwrap();
    store.record_message(NewMessage {
        room_id: "G".into(), sender_id: "bob".into(), sender_name: Some("Bob".into()),
        role: Role::User, body: "hi there".into(), ts: 200, personality: None, is_mention: false,
    }).await.unwrap();

    let llm = mock_llm();
    let permit = Arc::new(Semaphore::new(1));

    let n = run_sweep_once(&store, &llm, "test-model", &permit).await.unwrap();
    assert_eq!(n, 1);

    let summary = store.get_summary("G").await.unwrap().expect("summary should exist");
    assert_eq!(summary.summary, "[mock summary]");
    assert_eq!(summary.covered_through_ts, 200);

    // Second immediate sweep: no new messages since covered_through_ts, so nothing to do.
    let n2 = run_sweep_once(&store, &llm, "test-model", &permit).await.unwrap();
    assert_eq!(n2, 0);

    // A new message arrives; the room becomes due again.
    store.record_message(NewMessage {
        room_id: "G".into(), sender_id: "alice".into(), sender_name: Some("Alice".into()),
        role: Role::User, body: "one more thing".into(), ts: 300, personality: None, is_mention: false,
    }).await.unwrap();

    let n3 = run_sweep_once(&store, &llm, "test-model", &permit).await.unwrap();
    assert_eq!(n3, 1);

    let summary2 = store.get_summary("G").await.unwrap().unwrap();
    assert_eq!(summary2.covered_through_ts, 300);
}

/// A backlog bigger than the per-sweep fetch cap must never leave a gap:
/// covered_through_ts should only ever advance to the last message actually
/// fetched, so the room stays due and successive sweeps catch up chunk by
/// chunk until every message has been folded in — no ts is ever skipped over.
#[tokio::test]
async fn sweep_catches_up_incrementally_on_a_backlog_larger_than_the_cap() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.ensure_room("G", Some("Group"), true).await.unwrap();

    const TOTAL: i64 = 45; // just over RECENT_CAP (40): one full chunk + a small tail
    for i in 0..TOTAL {
        store.record_message(NewMessage {
            room_id: "G".into(), sender_id: "alice".into(), sender_name: Some("Alice".into()),
            role: Role::User, body: format!("msg {i}"), ts: 1000 + i, personality: None, is_mention: false,
        }).await.unwrap();
    }
    let true_max_ts = 1000 + TOTAL - 1;

    let llm = mock_llm();
    let permit = Arc::new(Semaphore::new(1));

    // First sweep: only the oldest 200 are fetched; covered_through_ts must land
    // on the last one actually summarized, NOT on the true max — and must still
    // be strictly less than the true max, since a backlog remains.
    let n1 = run_sweep_once(&store, &llm, "test-model", &permit).await.unwrap();
    assert_eq!(n1, 1);
    let after_first = store.get_summary("G").await.unwrap().unwrap().covered_through_ts;
    assert!(after_first < true_max_ts, "backlog remains: covered_through_ts must not jump past unseen messages");

    // Room is still due: the next sweep must pick up exactly where the last one
    // left off (no gap), fetching the remaining tail.
    let n2 = run_sweep_once(&store, &llm, "test-model", &permit).await.unwrap();
    assert_eq!(n2, 1);
    let after_second = store.get_summary("G").await.unwrap().unwrap().covered_through_ts;
    assert_eq!(after_second, true_max_ts, "second sweep must reach the true max ts with no skipped messages");

    // Fully caught up: no more sweeps are due until new activity arrives.
    let n3 = run_sweep_once(&store, &llm, "test-model", &permit).await.unwrap();
    assert_eq!(n3, 0);
}
