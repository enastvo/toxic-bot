//! Bot tool definitions and execution.
//!
//! Tools the LLM may call while composing a reply. Everything here is
//! constrained by our code: the model can only *request* a call; we validate
//! args and decide what actually runs. Tool output is returned to the model as
//! DATA, never instructions (prompt-injection containment).

use crate::search::{SearchParams, SearchProvider, SearchTopic};
use crate::store::Store;
use serde_json::{json, Value};

/// Everything a tool needs to run, assembled per turn by the router.
pub struct ToolCtx<'a> {
    pub store: &'a Store,
    pub room_id: &'a str,
    pub search: Option<&'a dyn SearchProvider>,
    /// Allowed domains for web search (already parsed from the settings string).
    pub whitelist: &'a [String],
}

/// Parse the settings whitelist string (comma / whitespace / newline separated)
/// into a deduped, lower-cased list of domains.
pub fn parse_whitelist(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tok in s.split(|c: char| c == ',' || c.is_whitespace()) {
        let d = tok.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_ascii_lowercase();
        if !d.is_empty() && !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// Merge the global whitelist string with a personality's extra domains into a
/// single deduped, normalized list. Extra domains apply only to that persona's
/// turns, so the shared whitelist stays clean.
pub fn merged_whitelist(global: &str, extra: &[String]) -> Vec<String> {
    let mut combined = global.to_string();
    for d in extra {
        combined.push(',');
        combined.push_str(d);
    }
    parse_whitelist(&combined)
}

/// The Ollama `tools` schema list offered to the model. `web_search` is only
/// included when web search is actually available (enabled + provider present).
pub fn tool_schemas(web_search: bool) -> Vec<Value> {
    let mut tools = vec![
        json!({
            "type": "function",
            "function": {
                "name": "calculator",
                "description": "Evaluate a basic arithmetic expression (+, -, *, /, parentheses, decimals). Use for any math.",
                "parameters": {
                    "type": "object",
                    "properties": { "expression": { "type": "string", "description": "e.g. \"(3.5 + 2) * 4\"" } },
                    "required": ["expression"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "current_time",
                "description": "Get the current date and time (UTC). Use when asked about the current time or date.",
                "parameters": { "type": "object", "properties": {} }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "room_search",
                "description": "Search THIS chat room's own past messages for a keyword. Use to recall what someone said earlier.",
                "parameters": {
                    "type": "object",
                    "properties": { "query": { "type": "string", "description": "keyword or phrase to look for" } },
                    "required": ["query"]
                }
            }
        }),
    ];
    if web_search {
        tools.push(json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web for current or factual information. Restricted to a curated set of trusted sites. Use when you need up-to-date or external facts.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "what to search for" },
                        "topic": {
                            "type": "string",
                            "enum": ["general", "news"],
                            "description": "Use \"news\" for current events, headlines, or anything time-sensitive (recent days); use \"general\" (default) for reference or factual lookups."
                        },
                        "days": {
                            "type": "integer",
                            "description": "For topic=news only: how many days back to search (default 7). Use a small number for 'today'/'latest'."
                        }
                    },
                    "required": ["query"]
                }
            }
        }));
    }
    tools
}

fn arg_u32(args: &Value, key: &str) -> Option<u32> {
    // Ollama usually sends numbers as JSON numbers, but tolerate a numeric string.
    let v = match args {
        Value::Object(_) => args.get(key).cloned(),
        Value::String(s) => serde_json::from_str::<Value>(s).ok().and_then(|v| v.get(key).cloned()),
        _ => None,
    }?;
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
        .map(|n| n as u32)
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    // Ollama returns `arguments` as an object; tolerate a JSON string too.
    match args {
        Value::Object(_) => args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string()),
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .and_then(|v| v.get(key).and_then(|x| x.as_str()).map(|x| x.to_string())),
        _ => None,
    }
}

/// Execute a tool call. Always returns a string to feed back to the model —
/// errors are returned as data (so a bad call never aborts the turn).
pub async fn execute(name: &str, args: &Value, ctx: &ToolCtx<'_>) -> String {
    match name {
        "calculator" => match arg_str(args, "expression") {
            Some(expr) => match eval_expr(&expr) {
                Ok(v) => format!("{expr} = {}", trim_float(v)),
                Err(e) => format!("Could not evaluate \"{expr}\": {e}"),
            },
            None => "calculator requires an \"expression\" argument.".into(),
        },
        "current_time" => {
            let now = time::OffsetDateTime::now_utc();
            format!(
                "Current UTC time: {}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
                now.year(), now.month() as u8, now.day(), now.hour(), now.minute(), now.second()
            )
        }
        "room_search" => match arg_str(args, "query") {
            Some(q) => match ctx.store.search_room(ctx.room_id, &q, 8).await {
                Ok(msgs) if msgs.is_empty() => format!("No messages in this room match \"{q}\"."),
                Ok(msgs) => {
                    let lines: Vec<String> = msgs
                        .iter()
                        .map(|m| {
                            let who = m.sender_name.clone().unwrap_or_else(|| m.sender_id.clone());
                            format!("{who}: {}", m.body)
                        })
                        .collect();
                    format!("Messages matching \"{q}\":\n{}", lines.join("\n"))
                }
                Err(_) => "room_search failed.".into(),
            },
            None => "room_search requires a \"query\" argument.".into(),
        },
        "web_search" => match arg_str(args, "query") {
            Some(q) => {
                let Some(provider) = ctx.search else {
                    return "Web search is not configured.".into();
                };
                if ctx.whitelist.is_empty() {
                    return "No whitelisted domains are configured, so web search is unavailable.".into();
                }
                let params = SearchParams {
                    topic: arg_str(args, "topic").map(|t| SearchTopic::parse(&t)).unwrap_or_default(),
                    days: arg_u32(args, "days"),
                };
                match provider.search(&q, ctx.whitelist, 5, &params).await {
                    Ok(results) if results.is_empty() => {
                        format!("No results found on the whitelisted sites for \"{q}\".")
                    }
                    Ok(results) => {
                        let blocks: Vec<String> = results
                            .iter()
                            .enumerate()
                            .map(|(i, r)| format!("{}. {} — {}\n{}", i + 1, r.title, r.url, r.content))
                            .collect();
                        format!("Web search results for \"{q}\":\n{}", blocks.join("\n\n"))
                    }
                    Err(e) => format!("Web search failed: {e}"),
                }
            }
            None => "web_search requires a \"query\" argument.".into(),
        },
        other => format!("Unknown tool: {other}"),
    }
}

fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        // trim trailing zeros
        let s = format!("{v:.6}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

// ---- tiny safe arithmetic evaluator (no eval, no deps) --------------------

/// Evaluate a basic arithmetic expression: + - * / , parentheses, decimals,
/// unary +/-. Rejects anything else. Never executes code.
pub fn eval_expr(input: &str) -> Result<f64, String> {
    let mut p = Parser { chars: input.chars().collect(), pos: 0 };
    p.skip_ws();
    let v = p.expr()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err("unexpected trailing characters".into());
    }
    Ok(v)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }
    // expr = term (('+' | '-') term)*
    fn expr(&mut self) -> Result<f64, String> {
        let mut v = self.term()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => { self.pos += 1; v += self.term()?; }
                Some('-') => { self.pos += 1; v -= self.term()?; }
                _ => break,
            }
        }
        Ok(v)
    }
    // term = factor (('*' | '/') factor)*
    fn term(&mut self) -> Result<f64, String> {
        let mut v = self.factor()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('*') => { self.pos += 1; v *= self.factor()?; }
                Some('/') => {
                    self.pos += 1;
                    let d = self.factor()?;
                    if d == 0.0 { return Err("division by zero".into()); }
                    v /= d;
                }
                _ => break,
            }
        }
        Ok(v)
    }
    // factor = number | '(' expr ')' | ('+'|'-') factor
    fn factor(&mut self) -> Result<f64, String> {
        self.skip_ws();
        match self.peek() {
            Some('+') => { self.pos += 1; self.factor() }
            Some('-') => { self.pos += 1; Ok(-self.factor()?) }
            Some('(') => {
                self.pos += 1;
                let v = self.expr()?;
                self.skip_ws();
                if self.peek() != Some(')') { return Err("missing ')'".into()); }
                self.pos += 1;
                Ok(v)
            }
            Some(c) if c.is_ascii_digit() || c == '.' => self.number(),
            Some(c) => Err(format!("unexpected character '{c}'")),
            None => Err("unexpected end of expression".into()),
        }
    }
    fn number(&mut self) -> Result<f64, String> {
        let start = self.pos;
        let mut seen_dot = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.pos += 1;
            } else if c == '.' && !seen_dot {
                seen_dot = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        s.parse::<f64>().map_err(|_| format!("invalid number \"{s}\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_basic() {
        assert_eq!(eval_expr("2+3").unwrap(), 5.0);
        assert_eq!(eval_expr("2 + 3 * 4").unwrap(), 14.0);
        assert_eq!(eval_expr("(2 + 3) * 4").unwrap(), 20.0);
        assert_eq!(eval_expr("-5 + 2").unwrap(), -3.0);
        assert_eq!(eval_expr("10 / 4").unwrap(), 2.5);
        assert_eq!(eval_expr("3.5 * 2").unwrap(), 7.0);
    }

    #[test]
    fn eval_rejects_garbage_and_div0() {
        assert!(eval_expr("2 +").is_err());
        assert!(eval_expr("rm -rf /").is_err());
        assert!(eval_expr("2 ; 3").is_err());
        assert!(eval_expr("1/0").is_err());
        assert!(eval_expr("(1+2").is_err());
    }

    #[test]
    fn merged_whitelist_adds_persona_domains_without_dupes() {
        let global = "wikipedia.org, reuters.com";
        let extra = vec!["breitbart.com".to_string(), "reuters.com".to_string()];
        let m = merged_whitelist(global, &extra);
        assert_eq!(m, vec!["wikipedia.org", "reuters.com", "breitbart.com"]);
        // no extras -> just the global set
        assert_eq!(merged_whitelist(global, &[]), vec!["wikipedia.org", "reuters.com"]);
    }

    #[test]
    fn whitelist_parses_and_dedupes() {
        let w = parse_whitelist("wikipedia.org, https://reuters.com/\nwikipedia.org  BBC.com");
        assert_eq!(w, vec!["wikipedia.org", "reuters.com", "bbc.com"]);
        assert!(parse_whitelist("   ").is_empty());
    }

    #[test]
    fn web_search_schema_gated() {
        assert_eq!(tool_schemas(false).len(), 3);
        assert_eq!(tool_schemas(true).len(), 4);
        let schemas = tool_schemas(true);
        let names: Vec<&str> = schemas
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"web_search"));
    }
}
