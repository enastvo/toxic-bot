use crate::types::{ChatTurn, Role};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String, pub system: String, pub turns: Vec<ChatTurn>,
    pub temperature: f32, pub top_p: f32, pub num_ctx: u32,
    pub repeat_penalty: f64, pub repeat_last_n: u32, pub num_predict: i32, pub keep_alive: String,
    /// Per-request generation timeout (seconds). Applied on the reqwest builder for
    /// this request so an edit in the web UI takes effect immediately (the client's
    /// construction-time timeout is only a safety fallback).
    pub ollama_timeout_secs: u64,
}

/// Build the `/api/chat` request body. `think: false` disables reasoning output on
/// thinking models (e.g. qwen3). Without it, qwen3 spends its token budget on
/// <think> and returns no visible content — which breaks the relevance-check JSON
/// and pollutes replies with reasoning traces (also much slower on CPU). All
/// current personalities use qwen3; Ollama ignores this for models that don't think.
pub(crate) fn build_chat_body(model: &str, msgs: &[serde_json::Value], opts_extra: &ChatRequest) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": msgs,
        "stream": false,
        "think": false,
        "keep_alive": opts_extra.keep_alive,
        "options": {
            "temperature": opts_extra.temperature,
            "top_p": opts_extra.top_p,
            "num_ctx": opts_extra.num_ctx,
            "repeat_penalty": opts_extra.repeat_penalty,
            "repeat_last_n": opts_extra.repeat_last_n,
            "num_predict": opts_extra.num_predict,
        }
    })
}

/// Map an Ollama `/api/chat` JSON response into `GenStats`. Missing fields default
/// to 0; never panics on malformed/partial input.
pub(crate) fn parse_gen_stats(v: &serde_json::Value) -> GenStats {
    GenStats {
        prompt_tokens: v["prompt_eval_count"].as_u64().unwrap_or(0) as u32,
        reply_tokens: v["eval_count"].as_u64().unwrap_or(0) as u32,
        total_ms: v["total_duration"].as_u64().unwrap_or(0) / 1_000_000,
        eval_ms: v["eval_duration"].as_u64().unwrap_or(0) / 1_000_000,
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Relevance { pub should_reply: bool, pub confidence: f32 }

pub fn parse_relevance(s: &str) -> Relevance {
    let (start, end) = (s.find('{'), s.rfind('}'));
    if let (Some(a), Some(b)) = (start, end) {
        if b > a {
            if let Ok(r) = serde_json::from_str::<Relevance>(&s[a..=b]) { return r; }
        }
    }
    Relevance { should_reply: false, confidence: 0.0 }
}

#[derive(Debug, Clone, Default)]
pub struct GenStats { pub prompt_tokens: u32, pub reply_tokens: u32, pub total_ms: u64, pub eval_ms: u64 }

/// A tool call the model requested in a `chat_step`.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub name: String,
    /// Raw arguments as returned by the model (usually a JSON object).
    pub arguments: serde_json::Value,
}

/// One assistant turn from `chat_step`: either final text (`tool_calls` empty)
/// or a request to call tools.
#[derive(Debug, Clone, Default)]
pub struct AssistantStep {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub stats: GenStats,
}

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn generate_reply(&self, req: ChatRequest) -> anyhow::Result<(String, GenStats)>;
    async fn relevance_check(&self, model: &str, num_ctx: u32, turns: Vec<ChatTurn>, timeout_secs: u64) -> anyhow::Result<Relevance>;
    async fn summarize(&self, model: &str, prior: &str, transcript: &str, timeout_secs: u64) -> anyhow::Result<String>;
    /// One chat round with an explicit message array and optional tool schemas.
    /// Returns the assistant's text and/or any tool calls it requested. The
    /// router uses this to drive the (bounded) tool-call loop.
    async fn chat_step(
        &self,
        messages: Vec<serde_json::Value>,
        tools: &[serde_json::Value],
        opts: &ChatRequest,
    ) -> anyhow::Result<AssistantStep>;
}

pub struct MockLlm { pub reply: String, pub relevance: Relevance }
#[async_trait]
impl LlmBackend for MockLlm {
    async fn generate_reply(&self, _req: ChatRequest) -> anyhow::Result<(String, GenStats)> { Ok((self.reply.clone(), GenStats::default())) }
    async fn relevance_check(&self, _m: &str, _c: u32, _t: Vec<ChatTurn>, _timeout_secs: u64) -> anyhow::Result<Relevance> { Ok(self.relevance) }
    async fn summarize(&self, _model: &str, _prior: &str, _transcript: &str, _timeout_secs: u64) -> anyhow::Result<String> { Ok("[mock summary]".to_string()) }
    async fn chat_step(&self, _messages: Vec<serde_json::Value>, _tools: &[serde_json::Value], _opts: &ChatRequest) -> anyhow::Result<AssistantStep> {
        Ok(AssistantStep { content: self.reply.clone(), ..Default::default() })
    }
}

/// Build the base `/api/chat` message array: the system prompt followed by the
/// labeled conversation turns.
pub(crate) fn base_messages(system: &str, turns: &[ChatTurn]) -> Vec<serde_json::Value> {
    let mut msgs = vec![serde_json::json!({"role":"system","content": system})];
    msgs.extend(turns.iter().map(turn_json));
    msgs
}

/// A model reported as currently loaded by Ollama's `/api/ps`.
#[derive(Debug, Clone, Serialize)]
pub struct OllamaModel {
    pub name: String,
    pub size_bytes: u64,
    pub expires_at: Option<String>,
}

/// Response of `GET /api/ps`: the models Ollama currently has loaded in memory.
#[derive(Debug, Clone, Serialize)]
pub struct OllamaPs {
    pub models: Vec<OllamaModel>,
}

/// Raw wire shape of a single model entry from Ollama's `/api/ps` response.
#[derive(Debug, Deserialize)]
struct RawOllamaModel {
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    expires_at: Option<String>,
}

/// Raw wire shape of `/api/ps`'s top-level response.
#[derive(Debug, Deserialize)]
struct RawOllamaPs {
    #[serde(default)]
    models: Vec<RawOllamaModel>,
}

pub struct OllamaClient { base_url: String, http: reqwest::Client }

impl OllamaClient {
    pub fn new(base_url: impl Into<String>, timeout_secs: u64) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .build().expect("reqwest client"),
        }
    }

    async fn chat_with_stats(&self, model: &str, msgs: Vec<serde_json::Value>, opts_extra: &ChatRequest)
        -> anyhow::Result<(String, GenStats)>
    {
        let body = build_chat_body(model, &msgs, opts_extra);
        let resp = self.http.post(format!("{}/api/chat", self.base_url))
            .timeout(std::time::Duration::from_secs(opts_extra.ollama_timeout_secs.max(1)))
            .json(&body).send().await?.error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        let content = v["message"]["content"].as_str().unwrap_or_default().trim();
        let stats = parse_gen_stats(&v);
        Ok((sanitize(content), stats))
    }

    /// GET `/api/ps` — the models Ollama currently has loaded. Callers (Task 17's
    /// health handler) degrade to `reachable: false` on `Err`.
    pub async fn ps(&self) -> anyhow::Result<OllamaPs> {
        let resp = self.http.get(format!("{}/api/ps", self.base_url))
            .send().await?.error_for_status()?;
        let raw: RawOllamaPs = resp.json().await?;
        Ok(OllamaPs {
            models: raw.models.into_iter().map(|m| OllamaModel {
                name: m.name,
                size_bytes: m.size,
                expires_at: m.expires_at,
            }).collect(),
        })
    }
}

const RELEVANCE_SYS: &str = "You decide whether the assistant should chime in UNPROMPTED to a group chat. \
Reply with ONLY a JSON object: {\"should_reply\": bool, \"confidence\": number 0..1}. \
Set should_reply true only if the assistant can add clear value right now.";

/// Neutral summarization prompt (spec §9) — deliberately NOT a room's personality
/// voice. Used to maintain the running per-room summary (topics, running jokes,
/// notable facts, who tends to say what) that gets merged with new messages.
const SUMMARY_SYS: &str = "Update the running summary of this group chat: ongoing topics, \
running jokes, notable facts, and who tends to say what. Merge the new messages into the \
prior summary. Keep it under ~200 words. Be neutral and factual.";

#[async_trait]
impl LlmBackend for OllamaClient {
    async fn generate_reply(&self, req: ChatRequest) -> anyhow::Result<(String, GenStats)> {
        let msgs = base_messages(&req.system, &req.turns);
        self.chat_with_stats(&req.model, msgs, &req).await
    }

    async fn chat_step(
        &self,
        messages: Vec<serde_json::Value>,
        tools: &[serde_json::Value],
        opts: &ChatRequest,
    ) -> anyhow::Result<AssistantStep> {
        let mut body = build_chat_body(&opts.model, &messages, opts);
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools.to_vec());
        }
        let resp = self.http.post(format!("{}/api/chat", self.base_url))
            .timeout(std::time::Duration::from_secs(opts.ollama_timeout_secs.max(1)))
            .json(&body).send().await?.error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        let msg = &v["message"];
        let content = sanitize(msg["content"].as_str().unwrap_or_default().trim());
        let mut tool_calls = Vec::new();
        if let Some(arr) = msg["tool_calls"].as_array() {
            for tc in arr {
                let f = &tc["function"];
                if let Some(name) = f["name"].as_str() {
                    tool_calls.push(ToolCall { name: name.to_string(), arguments: f["arguments"].clone() });
                }
            }
        }
        Ok(AssistantStep { content, tool_calls, stats: parse_gen_stats(&v) })
    }

    async fn relevance_check(&self, model: &str, num_ctx: u32, turns: Vec<ChatTurn>, timeout_secs: u64) -> anyhow::Result<Relevance> {
        let mut msgs = vec![serde_json::json!({"role":"system","content": RELEVANCE_SYS})];
        msgs.extend(turns.iter().map(turn_json));
        // small, dedicated body: fast/deterministic relevance gating, not routed through
        // build_chat_body since it doesn't carry a full ChatRequest's knobs. keep_alive
        // matches the default so the relevance gate also keeps the model resident.
        let body = serde_json::json!({
            "model": model, "messages": msgs, "stream": false, "think": false,
            "keep_alive": "30m",
            "options": {"temperature": 0.0, "num_ctx": num_ctx, "num_predict": 40}
        });
        let resp = self.http.post(format!("{}/api/chat", self.base_url))
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
            .json(&body).send().await?.error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        let raw = v["message"]["content"].as_str().unwrap_or_default().trim();
        Ok(parse_relevance(raw))
    }

    async fn summarize(&self, model: &str, prior: &str, transcript: &str, timeout_secs: u64) -> anyhow::Result<String> {
        let user_content = format!("Prior summary:\n{prior}\n\nNew messages:\n{transcript}");
        let msgs = vec![
            serde_json::json!({"role":"system","content": SUMMARY_SYS}),
            serde_json::json!({"role":"user","content": user_content}),
        ];
        let req = ChatRequest {
            model: model.to_string(), system: SUMMARY_SYS.to_string(), turns: vec![],
            temperature: 0.3, top_p: 0.9, num_ctx: 8192,
            repeat_penalty: 1.3, repeat_last_n: 256, num_predict: 300, keep_alive: "30m".to_string(),
            ollama_timeout_secs: timeout_secs.max(1),
        };
        let body = build_chat_body(model, &msgs, &req);
        let resp = self.http.post(format!("{}/api/chat", self.base_url))
            .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
            .json(&body).send().await?.error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        let content = v["message"]["content"].as_str().unwrap_or_default().trim();
        Ok(sanitize(content))
    }
}

// helper used by both real client and prompt building
pub(crate) fn turn_json(t: &ChatTurn) -> serde_json::Value {
    // Prefix only *other people's* turns with their `[Name]:` label (so the model
    // can attribute speakers in a group). The bot's OWN turns are left unlabeled:
    // its `assistant` role already identifies them, and labeling them
    // (`[you, as X]:`) taught the model to imitate the format and emit a
    // `[...]:` prefix in its replies. `sanitize()` strips any residual leak.
    let content = match &t.name {
        Some(n) if t.role == Role::User => format!("{n}: {}", t.content),
        _ => t.content.clone(),
    };
    serde_json::json!({ "role": t.role.as_str(), "content": content })
}

/// Strip qwen3 thinking artifacts / control tokens from model output before it is
/// stored or sent (prevents the /no_think self-reinforcement + echo loop).
/// Preserves internal newlines and paragraph structure; only collapses horizontal
/// whitespace within lines and removes excessive blank lines.
pub fn sanitize(raw: &str) -> String {
    // remove <think>...</think> (non-greedy, across newlines)
    let mut s = String::with_capacity(raw.len());
    let mut rest = raw;
    loop {
        match rest.find("<think>") {
            Some(start) => {
                s.push_str(&rest[..start]);
                match rest[start..].find("</think>") {
                    Some(end) => { rest = &rest[start + end + "</think>".len()..]; }
                    None => { break; }
                }
            }
            None => { s.push_str(rest); break; }
        }
    }

    // Process line-by-line: collapse horizontal whitespace, filter tokens, preserve newlines
    let lines: Vec<String> = s
        .lines()
        .map(|line| {
            line.split_whitespace()
                .filter(|tok| *tok != "/no_think" && *tok != "/think")
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();

    let mut result = lines.join("\n");

    // Collapse 3+ consecutive newlines to 2 (one blank line max)
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }

    strip_leading_speaker_label(result.trim())
}

/// Remove a single leading speaker-label prefix the model sometimes copies from
/// the labeled context into its own reply, e.g. `"[you, as Toxic Asshole]: hi"`
/// or `"[Zac Forristall]: hi"` -> `"hi"`. Only strips a short, single-line
/// `[label]:` at the very start, so ordinary text that happens to contain
/// brackets later is untouched.
fn strip_leading_speaker_label(s: &str) -> String {
    let t = s.trim_start();
    if let Some(rest) = t.strip_prefix('[') {
        if let Some(close) = rest.find("]:") {
            let label = &rest[..close];
            if !label.contains('\n') && label.chars().count() <= 60 {
                return rest[close + 2..].trim_start().to_string();
            }
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let r = parse_relevance(r#"{"should_reply": true, "confidence": 0.82}"#);
        assert!(r.should_reply); assert!((r.confidence - 0.82).abs() < 1e-6);
    }

    #[test]
    fn extracts_json_from_noise() {
        let r = parse_relevance("Sure!\n{\"should_reply\": false, \"confidence\": 0.1} \n");
        assert!(!r.should_reply);
    }

    #[test]
    fn junk_defaults_to_silent() {
        let r = parse_relevance("no idea");
        assert!(!r.should_reply); assert_eq!(r.confidence, 0.0);
    }

    #[test]
    fn sanitize_strips_think_block() {
        assert_eq!(sanitize("<think>reason</think>Hello"), "Hello");
        assert_eq!(sanitize("a <think>x\ny</think> b"), "a b");
    }

    #[test]
    fn sanitize_strips_control_tokens_whole_word_only() {
        assert_eq!(sanitize("Dude you just said /no_think again"), "Dude you just said again");
        assert_eq!(sanitize("/think then answer"), "then answer");
        // must NOT touch a real word containing the substring
        assert_eq!(sanitize("I think that rethink is fine"), "I think that rethink is fine");
    }

    #[test]
    fn sanitize_trims() {
        assert_eq!(sanitize("  hi  "), "hi");
    }

    #[test]
    fn sanitize_preserves_paragraph_newlines() {
        assert_eq!(sanitize("para one\npara two"), "para one\npara two");
        assert_eq!(sanitize("line one\n\n\nline two"), "line one\n\nline two");
    }

    #[test]
    fn sanitize_collapses_horizontal_runs_but_keeps_lines() {
        assert_eq!(sanitize("a    b\nc\t\td"), "a b\nc d");
    }

    #[test]
    fn sanitize_strips_leading_speaker_label() {
        // the two forms the model was leaking into replies
        assert_eq!(
            sanitize("[you, as Toxic Asshole]: Estefan, welcome back"),
            "Estefan, welcome back"
        );
        assert_eq!(
            sanitize("[Zac Forristall]: Aww, thanks sweetheart"),
            "Aww, thanks sweetheart"
        );
        // only the leading label is removed; brackets later in the text stay
        assert_eq!(sanitize("no label here [not a label]: keep"), "no label here [not a label]: keep");
        // a normal reply with no label is unchanged
        assert_eq!(sanitize("just a normal reply"), "just a normal reply");
    }

    #[test]
    fn chat_body_has_think_false_and_knobs() {
        let req = ChatRequest { model:"qwen3:8b".into(), system:"s".into(), turns:vec![],
            temperature:0.7, top_p:0.9, num_ctx:8192, repeat_penalty:1.3, repeat_last_n:256,
            num_predict:512, keep_alive:"30m".into(), ollama_timeout_secs:300 };
        let msgs = vec![serde_json::json!({"role":"system","content":"s"})];
        let body = build_chat_body(&req.model, &msgs, &req);
        assert_eq!(body["think"], serde_json::json!(false));
        assert_eq!(body["keep_alive"], serde_json::json!("30m"));
        assert_eq!(body["options"]["num_predict"], serde_json::json!(512));
        assert_eq!(body["options"]["repeat_penalty"], serde_json::json!(1.3));
        assert_eq!(body["options"]["repeat_last_n"], serde_json::json!(256));
        assert_eq!(body["stream"], serde_json::json!(false));
    }

    #[test]
    fn parse_gen_stats_maps_ollama_fields() {
        let v = serde_json::json!({"message":{"content":"hi"},"prompt_eval_count":123,"eval_count":45,"total_duration":2_000_000_000u64,"eval_duration":1_500_000_000u64});
        let s = parse_gen_stats(&v);
        assert_eq!(s.prompt_tokens, 123);
        assert_eq!(s.reply_tokens, 45);
        assert_eq!(s.total_ms, 2000);
        assert_eq!(s.eval_ms, 1500);
    }
}
