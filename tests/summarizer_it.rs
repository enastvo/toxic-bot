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
