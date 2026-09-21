//! Whitelist-restricted web search provider (Tavily).
//!
//! The provider does the fetching server-side and returns cleaned, extracted
//! text — the bot never fetches raw HTML itself, which removes the SSRF surface
//! and most of the malware risk. Results are still *untrusted data* (possible
//! prompt-injection), so callers must treat them as data, never instructions.

use async_trait::async_trait;
use serde::Deserialize;

/// One search hit with provider-extracted text.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub content: String,
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Search `query`, restricted to `include_domains` (the operator whitelist),
    /// returning at most `max_results` hits.
    async fn search(
        &self,
        query: &str,
        include_domains: &[String],
        max_results: usize,
    ) -> anyhow::Result<Vec<SearchResult>>;
}

/// Tavily (https://tavily.com) — an LLM-oriented search API that accepts an
/// `include_domains` allowlist and returns cleaned content.
pub struct TavilyClient {
    api_key: String,
    http: reqwest::Client,
}

impl TavilyClient {
    pub fn new(api_key: String, timeout_secs: u64) -> Self {
        Self {
            api_key,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
                .build()
                .expect("reqwest client"),
        }
    }
}

#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

#[async_trait]
impl SearchProvider for TavilyClient {
    async fn search(
        &self,
        query: &str,
        include_domains: &[String],
        max_results: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let body = serde_json::json!({
            "api_key": self.api_key,
            "query": query,
            "include_domains": include_domains,
            "max_results": max_results,
            "search_depth": "basic",
        });
        let resp = self
            .http
            .post("https://api.tavily.com/search")
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        let parsed: TavilyResponse = resp.json().await?;
        Ok(parsed
            .results
            .into_iter()
            .map(|r| SearchResult { title: r.title, url: r.url, content: r.content })
            .collect())
    }
}
