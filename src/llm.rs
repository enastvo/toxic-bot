use crate::types::{ChatTurn, Role};
use async_trait::async_trait;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String, pub system: String, pub turns: Vec<ChatTurn>,
    pub temperature: f32, pub top_p: f32, pub num_ctx: u32,
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

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn generate_reply(&self, req: ChatRequest) -> anyhow::Result<String>;
    async fn relevance_check(&self, model: &str, num_ctx: u32, turns: Vec<ChatTurn>) -> anyhow::Result<Relevance>;
}

pub struct MockLlm { pub reply: String, pub relevance: Relevance }
#[async_trait]
impl LlmBackend for MockLlm {
    async fn generate_reply(&self, _req: ChatRequest) -> anyhow::Result<String> { Ok(self.reply.clone()) }
    async fn relevance_check(&self, _m: &str, _c: u32, _t: Vec<ChatTurn>) -> anyhow::Result<Relevance> { Ok(self.relevance) }
}

pub struct OllamaClient { base_url: String, http: reqwest::Client }

impl OllamaClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build().expect("reqwest client"),
        }
    }

    async fn chat(&self, model: &str, msgs: Vec<serde_json::Value>, opts: serde_json::Value)
        -> anyhow::Result<String>
    {
        // `think: false` disables reasoning output on thinking models (e.g. qwen3).
        // Without it, qwen3 spends its token budget on <think> and returns no visible
        // content — which breaks the relevance-check JSON and pollutes replies with
        // reasoning traces (also much slower on CPU). All current personalities use
        // qwen3; Ollama ignores this for models that don't think.
        let body = serde_json::json!({ "model": model, "messages": msgs, "stream": false, "think": false, "options": opts });
        let resp = self.http.post(format!("{}/api/chat", self.base_url))
            .json(&body).send().await?.error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        Ok(v["message"]["content"].as_str().unwrap_or_default().trim().to_string())
    }
}

const RELEVANCE_SYS: &str = "You decide whether the assistant should chime in UNPROMPTED to a group chat. \
Reply with ONLY a JSON object: {\"should_reply\": bool, \"confidence\": number 0..1}. \
Set should_reply true only if the assistant can add clear value right now.";

#[async_trait]
impl LlmBackend for OllamaClient {
    async fn generate_reply(&self, req: ChatRequest) -> anyhow::Result<String> {
        let mut msgs = vec![serde_json::json!({"role":"system","content": req.system})];
        msgs.extend(req.turns.iter().map(turn_json));
        let opts = serde_json::json!({"temperature": req.temperature, "top_p": req.top_p, "num_ctx": req.num_ctx});
        self.chat(&req.model, msgs, opts).await
    }

    async fn relevance_check(&self, model: &str, num_ctx: u32, turns: Vec<ChatTurn>) -> anyhow::Result<Relevance> {
        let mut msgs = vec![serde_json::json!({"role":"system","content": RELEVANCE_SYS})];
        msgs.extend(turns.iter().map(turn_json));
        let opts = serde_json::json!({"temperature": 0.0, "num_ctx": num_ctx, "num_predict": 40});
        let raw = self.chat(model, msgs, opts).await?;
        Ok(parse_relevance(&raw))
    }
}

// helper used by both real client and prompt building
pub(crate) fn turn_json(t: &ChatTurn) -> serde_json::Value {
    let content = match &t.name {
        Some(n) if t.role == Role::User => format!("{n}: {}", t.content),
        _ => t.content.clone(),
    };
    serde_json::json!({ "role": t.role.as_str(), "content": content })
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
}
