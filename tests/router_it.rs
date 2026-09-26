use signal_bot::llm::{MockLlm, Relevance};
use signal_bot::metrics::Metrics;
use signal_bot::personalities::Personalities;
use signal_bot::router::Router;
use signal_bot::signal::MockSignal;
use signal_bot::store::{SettingsRow, Store};
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

/// Seed a valid default settings row (Global-Constraints defaults) so that
/// `store.get_settings()` succeeds in the fresh in-memory stores these tests use.
async fn seed_settings(store: &Store) {
    store
        .upsert_settings(&SettingsRow {
            keep_alive: "30m".into(),
            ollama_timeout_secs: 300,
            repeat_penalty: 1.3,
            repeat_last_n: 256,
            num_predict: 512,
            num_ctx: 8192,
            default_temperature: 0.7,
            default_top_p: 0.9,
            summary_enabled: true,
            summary_interval_hours: 6,
            tools_enabled: false,
            web_search_enabled: false,
            search_whitelist: "wikipedia.org".into(),
            max_tool_rounds: 2,
        })
        .await
        .unwrap();
}

fn incoming(room: &str, group: bool, mention: bool) -> IncomingMessage {
    IncomingMessage { room_id: room.into(), sender_id: "+1000".into(), sender_name: Some("Alice".into()),
        body: "hello bot".into(), is_group: group, is_mention: mention, quoted_msg: None, timestamp: 1 }
}

#[tokio::test]
async fn direct_message_gets_reply_and_is_sent() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_settings(&store).await;
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "hi there".into(), relevance: Relevance{should_reply:false, confidence:0.0} });
    let r = Router::new(store.clone(), personalities(), llm, sig.clone(), "+bot".into(), false, Metrics::new(), None, None, vec![]);

    let out = r.handle(incoming("+1000", false, false)).await.unwrap();
    assert_eq!(out.as_deref(), Some("hi there"));
    assert_eq!(sig.sent.lock().unwrap().len(), 1);
    // both the incoming and the assistant reply are stored
    assert_eq!(store.recent("+1000", 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn group_addressed_silent_without_mention() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_settings(&store).await;
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "x".into(), relevance: Relevance{should_reply:true, confidence:1.0} });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), false, Metrics::new(), None, None, vec![]);
    let out = r.handle(incoming("G", true, false)).await.unwrap();
    assert!(out.is_none());
    assert!(sig.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn proactive_below_threshold_stays_silent() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_settings(&store).await;
    store.ensure_room("G", Some("Grp"), true).await.unwrap();
    store.set_reply_mode("G", ReplyMode::Proactive).await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "x".into(), relevance: Relevance{should_reply:true, confidence:0.5} });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), false, Metrics::new(), None, None, vec![]);
    let out = r.handle(incoming("G", true, false)).await.unwrap();
    assert!(out.is_none()); // 0.5 < 0.7 threshold
    assert!(sig.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dry_run_proactive_does_not_consume_rate_limit() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_settings(&store).await;
    store.ensure_room("G", Some("Grp"), true).await.unwrap();
    store.set_reply_mode("G", ReplyMode::Proactive).await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "would say".into(), relevance: Relevance{should_reply:true, confidence:1.0} });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), true, Metrics::new(), None, None, vec![]);

    let out1 = r.handle(incoming("G", true, false)).await.unwrap();
    let out2 = r.handle(incoming("G", true, false)).await.unwrap();

    // default personality's proactive cooldown_secs=60 would block the 2nd call
    // within the same second if dry-run were consuming the rate limiter.
    assert_eq!(out1.as_deref(), Some("would say"));
    assert_eq!(out2.as_deref(), Some("would say"));
    assert!(sig.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn self_sent_message_is_ignored() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_settings(&store).await;
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "should not be used".into(), relevance: Relevance{should_reply:true, confidence:1.0} });
    let r = Router::new(store.clone(), personalities(), llm, sig.clone(), "+bot".into(), false, Metrics::new(), None, None, vec![]);

    let mut msg = incoming("+1000", false, false);
    msg.sender_id = "+bot".into();
    let out = r.handle(msg).await.unwrap();

    assert!(out.is_none());
    assert!(sig.sent.lock().unwrap().is_empty());
    // the self-sent message must not even be recorded
    assert_eq!(store.recent("+1000", 10).await.unwrap().len(), 0);
}

#[tokio::test]
async fn dry_run_produces_reply_but_does_not_send() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_settings(&store).await;
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "would say".into(), relevance: Relevance{should_reply:false, confidence:0.0} });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), true, Metrics::new(), None, None, vec![]);
    let out = r.handle(incoming("+1000", false, false)).await.unwrap();
    assert_eq!(out.as_deref(), Some("would say"));
    assert!(sig.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn direct_reply_records_one_metrics_reply() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_settings(&store).await;
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "hi there".into(), relevance: Relevance{should_reply:false, confidence:0.0} });
    let metrics = Metrics::new();
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), false, metrics.clone(), None, None, vec![]);

    let out = r.handle(incoming("+1000", false, false)).await.unwrap();
    assert_eq!(out.as_deref(), Some("hi there"));
    assert_eq!(metrics.snapshot().replies, 1);
}

/// A transport whose sends always fail (e.g. signal-cli socket disconnected).
struct FailingSignal;
#[async_trait::async_trait]
impl signal_bot::signal::SignalTransport for FailingSignal {
    async fn send(&self, _room_id: &str, _is_group: bool, _text: &str) -> anyhow::Result<()> {
        anyhow::bail!("signal-cli socket is not connected")
    }
}

#[tokio::test]
async fn failed_send_is_not_recorded_as_a_bot_message() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_settings(&store).await;
    let llm = Arc::new(MockLlm { reply: "hi there".into(), relevance: Relevance{should_reply:false, confidence:0.0} });
    let metrics = Metrics::new();
    let r = Router::new(store.clone(), personalities(), llm, Arc::new(FailingSignal), "+bot".into(), false, metrics.clone(), None, None, vec![]);

    assert!(r.handle(incoming("+1000", false, false)).await.is_err());
    // Only the incoming message is stored; the undelivered reply is not.
    let hist = store.recent("+1000", 10).await.unwrap();
    assert_eq!(hist.len(), 1);
    assert_eq!(hist[0].body, "hello bot");
    assert_eq!(metrics.snapshot().errors, 1);
}

/// Counts relevance checks so we can prove the (cheap) rate limit is consulted
/// before the (expensive) relevance model call.
struct CountingLlm {
    relevance_calls: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl signal_bot::llm::LlmBackend for CountingLlm {
    async fn generate_reply(&self, _r: signal_bot::llm::ChatRequest) -> anyhow::Result<(String, signal_bot::llm::GenStats)> {
        Ok(("chiming in".into(), Default::default()))
    }
    async fn relevance_check(&self, _m: &str, _c: u32, _t: Vec<signal_bot::types::ChatTurn>, _to: u64) -> anyhow::Result<Relevance> {
        self.relevance_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Relevance { should_reply: true, confidence: 1.0 })
    }
    async fn summarize(&self, _m: &str, _p: &str, _t: &str, _to: u64) -> anyhow::Result<String> {
        Ok(String::new())
    }
    async fn chat_step(&self, _m: Vec<serde_json::Value>, _t: &[serde_json::Value], _o: &signal_bot::llm::ChatRequest) -> anyhow::Result<signal_bot::llm::AssistantStep> {
        Ok(Default::default())
    }
}

#[tokio::test]
async fn proactive_cooldown_skips_relevance_call() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_settings(&store).await;
    store.ensure_room("G", Some("Grp"), true).await.unwrap();
    store.set_reply_mode("G", ReplyMode::Proactive).await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(CountingLlm { relevance_calls: Default::default() });
    let r = Router::new(store, personalities(), llm.clone(), sig.clone(), "+bot".into(), false, Metrics::new(), None, None, vec![]);

    // First unaddressed message: relevance runs, bot chimes in (starts cooldown).
    assert_eq!(r.handle(incoming("G", true, false)).await.unwrap().as_deref(), Some("chiming in"));
    // Second, within the 60s cooldown: rate-limited WITHOUT a relevance call.
    assert!(r.handle(incoming("G", true, false)).await.unwrap().is_none());
    assert_eq!(llm.relevance_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(sig.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn dm_reply_publishes_bot_sse_event() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    seed_settings(&store).await;
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "hi there".into(), relevance: Relevance{should_reply:false, confidence:0.0} });
    let (tx, mut rx) = tokio::sync::broadcast::channel(16);
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), false, Metrics::new(), Some(tx), None, vec![]);

    let out = r.handle(incoming("+1000", false, false)).await.unwrap();
    assert_eq!(out.as_deref(), Some("hi there"));

    // First event is the incoming user message; second is the bot's reply.
    let evt1 = rx.try_recv().unwrap();
    assert_eq!(evt1.sender, "Alice");
    let evt2 = rx.try_recv().unwrap();
    assert_eq!(evt2.sender, "bot");
    assert_eq!(evt2.body, "hi there");
    assert_eq!(evt2.room_id, "+1000");
}
