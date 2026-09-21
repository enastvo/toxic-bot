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

/// Which Tavily search corpus to hit. `News` restricts to recent news content
/// (and enables the `days` recency window); `General` is the default web corpus,
/// which is what you want for reference/factual lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchTopic {
    #[default]
    General,
    News,
}

impl SearchTopic {
    pub fn as_str(self) -> &'static str {
        match self {
            SearchTopic::General => "general",
            SearchTopic::News => "news",
        }
    }
    /// Parse the model-supplied `topic` argument; anything that isn't "news"
    /// (case-insensitive) falls back to the safe default, `General`.
    pub fn parse(s: &str) -> SearchTopic {
        if s.trim().eq_ignore_ascii_case("news") { SearchTopic::News } else { SearchTopic::General }
    }
}

/// Tunables the model can set per call to steer result quality. `days` only
/// applies to `News` (how far back to look); it is ignored for `General`.
#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub topic: SearchTopic,
    pub days: Option<u32>,
}

#[async_trait]
pub trait SearchProvider: Send + Sync {
    /// Search `query`, restricted to `include_domains` (the operator whitelist),
    /// returning at most `max_results` hits. `params` steers depth/recency.
    async fn search(
        &self,
        query: &str,
        include_domains: &[String],
        max_results: usize,
        params: &SearchParams,
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

/// Default recency window (days) for a `News` search when the caller didn't
/// specify one — without it Tavily returns stale/undated results.
const DEFAULT_NEWS_DAYS: u32 = 7;

/// Build the Tavily `/search` request body. Kept pure and separate from the HTTP
/// call so the request shape (depth/topic/recency) is unit-testable. We always
/// use `advanced` depth (basic returns index/archive pages for topical queries);
/// `News` adds a `days` recency window so "today's headlines" isn't answered with
/// years-old archive pages.
pub(crate) fn build_search_body(
    api_key: &str,
    query: &str,
    include_domains: &[String],
    max_results: usize,
    params: &SearchParams,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "api_key": api_key,
        "query": query,
        "include_domains": include_domains,
        "max_results": max_results,
        "search_depth": "advanced",
        "topic": params.topic.as_str(),
    });
    if params.topic == SearchTopic::News {
        let days = params.days.unwrap_or(DEFAULT_NEWS_DAYS).max(1);
        body["days"] = serde_json::json!(days);
    }
    body
}

#[async_trait]
impl SearchProvider for TavilyClient {
    async fn search(
        &self,
        query: &str,
        include_domains: &[String],
        max_results: usize,
        params: &SearchParams,
    ) -> anyhow::Result<Vec<SearchResult>> {
        let body = build_search_body(&self.api_key, query, include_domains, max_results, params);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_parse_defaults_to_general() {
        assert_eq!(SearchTopic::parse("news"), SearchTopic::News);
        assert_eq!(SearchTopic::parse("NEWS"), SearchTopic::News);
        assert_eq!(SearchTopic::parse("general"), SearchTopic::General);
        assert_eq!(SearchTopic::parse("garbage"), SearchTopic::General);
        assert_eq!(SearchTopic::parse(""), SearchTopic::General);
    }

    #[test]
    fn general_body_uses_advanced_depth_and_no_days() {
        let domains = vec!["reuters.com".to_string(), "bbc.com".to_string()];
        let b = build_search_body("k", "rust language", &domains, 5, &SearchParams::default());
        assert_eq!(b["query"], "rust language");
        assert_eq!(b["max_results"], 5);
        assert_eq!(b["search_depth"], "advanced");
        assert_eq!(b["topic"], "general");
        assert_eq!(b["include_domains"], serde_json::json!(["reuters.com", "bbc.com"]));
        // days is meaningless for general search and must be omitted
        assert!(b.get("days").is_none(), "general search must not send days: {b}");
    }

    #[test]
    fn news_body_sets_topic_and_days_window() {
        let params = SearchParams { topic: SearchTopic::News, days: Some(3) };
        let b = build_search_body("k", "headlines today", &[], 4, &params);
        assert_eq!(b["topic"], "news");
        assert_eq!(b["days"], 3);
        assert_eq!(b["search_depth"], "advanced");
    }

    #[test]
    fn news_without_explicit_days_defaults_recency_window() {
        let params = SearchParams { topic: SearchTopic::News, days: None };
        let b = build_search_body("k", "headlines", &[], 5, &params);
        assert_eq!(b["topic"], "news");
        // a news search with no window would return stale results; default one in
        assert!(b["days"].as_u64().unwrap() >= 1, "news search should default a days window: {b}");
    }
}
