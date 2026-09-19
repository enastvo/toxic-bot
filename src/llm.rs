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
