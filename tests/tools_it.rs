use async_trait::async_trait;
use serde_json::{json, Value};
use signal_bot::llm::{AssistantStep, ChatRequest, GenStats, LlmBackend, Relevance, ToolCall};
use signal_bot::personalities::Personalities;
use signal_bot::router::Router;
use signal_bot::search::{SearchProvider, SearchResult};
use signal_bot::signal::MockSignal;
use signal_bot::store::{SettingsRow, Store};
use signal_bot::tools::{execute, ToolCtx};
use signal_bot::types::IncomingMessage;
use std::io::Write;
use std::sync::{Arc, Mutex};

fn personalities() -> Arc<Personalities> {
    let d = tempfile::tempdir().unwrap();
    let body = "label=\"D\"\nsystem_prompt=\"be nice\"\nmodel=\"m\"\ntemperature=0.5\ntop_p=0.9\nnum_ctx=4096\n[proactive]\nrelevance_threshold=0.7\ncooldown_secs=60\nmax_per_hour=5\n";
    std::fs::File::create(d.path().join("default.toml")).unwrap().write_all(body.as_bytes()).unwrap();
    let p = Arc::new(Personalities::load_dir(d.path()).unwrap());
    std::mem::forget(d);
    p
}

fn settings(tools_enabled: bool) -> SettingsRow {
    SettingsRow {
        keep_alive: "30m".into(), ollama_timeout_secs: 300, repeat_penalty: 1.3, repeat_last_n: 256,
        num_predict: 512, num_ctx: 8192, default_temperature: 0.7, default_top_p: 0.9,
        summary_enabled: false, summary_interval_hours: 6,
        tools_enabled, web_search_enabled: false, search_whitelist: "wikipedia.org".into(),
        max_tool_rounds: 2,
    }
}

/// Fake backend that, when offered tools, asks for `calculator("2+2")` on the
/// first step, then on the second step echoes back the last tool result it was
/// given (proving the tool actually ran and the result was fed back).
struct ToolLoopLlm {
    round: Mutex<usize>,
}
#[async_trait]
impl LlmBackend for ToolLoopLlm {
    async fn generate_reply(&self, _req: ChatRequest) -> anyhow::Result<(String, GenStats)> {
        Ok(("no tools path".into(), GenStats::default()))
    }
    async fn relevance_check(&self, _m: &str, _c: u32, _t: Vec<signal_bot::types::ChatTurn>, _to: u64) -> anyhow::Result<Relevance> {
        Ok(Relevance { should_reply: false, confidence: 0.0 })
    }
    async fn summarize(&self, _m: &str, _p: &str, _t: &str, _to: u64) -> anyhow::Result<String> {
        Ok(String::new())
    }
    async fn chat_step(&self, messages: Vec<Value>, tools: &[Value], _opts: &ChatRequest) -> anyhow::Result<AssistantStep> {
        let mut r = self.round.lock().unwrap();
        let this = *r;
        *r += 1;
        if this == 0 {
            assert!(!tools.is_empty(), "tools should be offered on the first round");
            return Ok(AssistantStep {
                content: String::new(),
                tool_calls: vec![ToolCall { name: "calculator".into(), arguments: json!({"expression": "2+2"}) }],
                stats: GenStats::default(),
            });
        }
        // second round: return the last tool-result message as the final reply
        let last_tool = messages.iter().rev()
            .find(|m| m["role"] == "tool")
            .and_then(|m| m["content"].as_str())
            .unwrap_or("<none>")
            .to_string();
        Ok(AssistantStep { content: last_tool, tool_calls: vec![], stats: GenStats::default() })
    }
}

#[tokio::test]
async fn tool_loop_runs_calculator_and_returns_final_reply() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.upsert_settings(&settings(true)).await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(ToolLoopLlm { round: Mutex::new(0) });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), false,
                        signal_bot::metrics::Metrics::new(), None, None);

    let msg = IncomingMessage { room_id: "+1000".into(), sender_id: "+1000".into(),
        sender_name: Some("Alice".into()), body: "what is 2+2".into(), is_group: false,
        is_mention: false, quoted_msg: None, timestamp: 1 };
    let out = r.handle(msg).await.unwrap();

    // final reply is the calculator's result, fed back through the loop
    assert_eq!(out.as_deref(), Some("2+2 = 4"));
    assert_eq!(sig.sent.lock().unwrap().len(), 1);
}

/// Records the domains it was asked to restrict to, so we can assert the
/// whitelist is passed through to the provider.
struct RecordingSearch {
    seen_domains: Mutex<Vec<String>>,
}
#[async_trait]
impl SearchProvider for RecordingSearch {
    async fn search(&self, _q: &str, include_domains: &[String], _max: usize) -> anyhow::Result<Vec<SearchResult>> {
        *self.seen_domains.lock().unwrap() = include_domains.to_vec();
        Ok(vec![SearchResult { title: "Rust".into(), url: "https://en.wikipedia.org/wiki/Rust".into(), content: "A language.".into() }])
    }
}

#[tokio::test]
async fn web_search_passes_whitelist_and_formats_results() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let provider = RecordingSearch { seen_domains: Mutex::new(vec![]) };
    let whitelist = vec!["en.wikipedia.org".to_string(), "reuters.com".to_string()];
    let ctx = ToolCtx { store: &store, room_id: "r", search: Some(&provider), whitelist: &whitelist };

    let out = execute("web_search", &json!({"query": "rust language"}), &ctx).await;
    assert!(out.contains("en.wikipedia.org"), "result should include the url: {out}");
    assert!(out.contains("A language."), "result should include content: {out}");
    // the whitelist was passed to the provider as include_domains
    assert_eq!(*provider.seen_domains.lock().unwrap(), whitelist);
}

#[tokio::test]
async fn web_search_unavailable_without_provider() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let ctx = ToolCtx { store: &store, room_id: "r", search: None, whitelist: &["wikipedia.org".to_string()] };
    let out = execute("web_search", &json!({"query": "x"}), &ctx).await;
    assert!(out.to_lowercase().contains("not configured"), "got: {out}");
}
