use signal_bot::llm::{MockLlm, Relevance};
use signal_bot::personalities::Personalities;
use signal_bot::router::Router;
use signal_bot::signal::MockSignal;
use signal_bot::store::Store;
use signal_bot::types::{IncomingMessage, ReplyMode};
use std::sync::Arc;
use std::io::Write;

fn personalities() -> Arc<Personalities> {
    let d = tempfile::tempdir().unwrap();
    let body = "label=\"D\"\nsystem_prompt=\"be nice\"\nmodel=\"m\"\ntemperature=0.5\ntop_p=0.9\nnum_ctx=4096\n[proactive]\nrelevance_threshold=0.7\ncooldown_secs=60\nmax_per_hour=5\n";
    std::fs::File::create(d.path().join("default.toml")).unwrap().write_all(body.as_bytes()).unwrap();
    let p = Arc::new(Personalities::load_dir(d.path()).unwrap());
    std::mem::forget(d); // keep temp dir alive for the test process
    p
}

fn incoming(room: &str, group: bool, mention: bool) -> IncomingMessage {
    IncomingMessage { room_id: room.into(), sender_id: "+1000".into(), sender_name: Some("Alice".into()),
        body: "hello bot".into(), is_group: group, is_mention: mention, quoted_msg: None, timestamp: 1 }
}

#[tokio::test]
async fn direct_message_gets_reply_and_is_sent() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "hi there".into(), relevance: Relevance{should_reply:false, confidence:0.0} });
    let r = Router::new(store.clone(), personalities(), llm, sig.clone(), "+bot".into(), false);

    let out = r.handle(incoming("+1000", false, false)).await.unwrap();
    assert_eq!(out.as_deref(), Some("hi there"));
    assert_eq!(sig.sent.lock().unwrap().len(), 1);
    // both the incoming and the assistant reply are stored
    assert_eq!(store.recent("+1000", 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn group_addressed_silent_without_mention() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "x".into(), relevance: Relevance{should_reply:true, confidence:1.0} });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), false);
    let out = r.handle(incoming("G", true, false)).await.unwrap();
    assert!(out.is_none());
    assert!(sig.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn proactive_below_threshold_stays_silent() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.ensure_room("G", Some("Grp"), true).await.unwrap();
    store.set_reply_mode("G", ReplyMode::Proactive).await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "x".into(), relevance: Relevance{should_reply:true, confidence:0.5} });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), false);
    let out = r.handle(incoming("G", true, false)).await.unwrap();
    assert!(out.is_none()); // 0.5 < 0.7 threshold
    assert!(sig.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dry_run_produces_reply_but_does_not_send() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "would say".into(), relevance: Relevance{should_reply:false, confidence:0.0} });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), true);
    let out = r.handle(incoming("+1000", false, false)).await.unwrap();
    assert_eq!(out.as_deref(), Some("would say"));
    assert!(sig.sent.lock().unwrap().is_empty());
}
