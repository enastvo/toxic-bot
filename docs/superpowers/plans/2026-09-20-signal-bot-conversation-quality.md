# Signal Bot Conversation-Quality / Orchestration / Observability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the bot's conversational architecture (template collapse, speaker confusion, stale/duplicate replies, `/no_think` leak, insult-instead-of-answer), give the operator web-editable LLM controls + a health dashboard, and add per-room long-term summary memory.

**Architecture:** Per-room actor tasks coalesce message bursts into one reply and route all LLM calls through a single global inference permit (one qwen3 generation at a time). Model output is sanitized before it is ever stored. Context is assembled in layers (summary + trimmed window with capped bot self-history + current burst) with explicit `[Name]:` speaker labels. LLM knobs live in a DB-backed settings table (web-editable), seeded from `config.toml`, with per-personality TOML overrides. Personality prompts split shared "house rules" (in code) from per-personality "character".

**Tech Stack:** Rust, tokio (tasks, mpsc, Semaphore), sqlx (SQLite), axum + askama + tower-sessions, reqwest (Ollama), serde/toml.

**Spec:** `docs/superpowers/specs/2026-09-20-signal-bot-conversation-quality-design.md`

## Global Constraints

- Existing crate `signal-bot`, edition 2021, on `master`. Keep the established patterns (sqlx runtime `query()`, `#[cfg(test)]` unit tests, `tests/*_it.rs` integration, `MockLlm`/`MockSignal`, `Arc<dyn LlmBackend>`/`Arc<dyn SignalTransport>`).
- Model stays `qwen3:8b`; all Ollama calls keep `think: false`.
- At most **one LLM generation process-wide** (global `Semaphore(1)`); **one in-flight turn per room**; bursts **coalesce into one reply**; dedupe by Signal message timestamp.
- Every model reply is **sanitized before it is stored or sent** (strip `<think>…</think>`, whole-token `/no_think` and `/think`, trim).
- Settings live in a **single-row `settings` DB table** (typed columns), seeded from `config.toml`; **precedence = per-personality TOML override → DB global**. Defaults: `keep_alive="30m"`, `ollama_timeout_secs=300`, `repeat_penalty=1.3`, `repeat_last_n=256`, `num_predict=512`, `num_ctx=8192`, `default_temperature=0.7`, `default_top_p=0.9`, `summary_enabled=true`, `summary_interval_hours=6`.
- Bounds (validate on write): timeout 10–600, num_predict 16–4096, num_ctx 512–32768, repeat_penalty 0.5–2.0, repeat_last_n 0–2048, temperature 0.0–2.0, top_p 0.0–1.0, summary_interval_hours 1–168.
- Context: trimmed window ~15–20 messages, **bot's own replies capped at the last 2**; speaker label format is exactly `[Name]: text` for humans and `[you, as <Label>]: text` for the bot's retained turns.
- Web additions behind the existing admin login; metrics are **in-memory** (reset on restart) in v1.
- Run `cargo test` + `cargo clippy --all-targets -- -D warnings` green before each commit. TDD: real captured RED before implementation.

## File Structure

```
src/
  llm.rs          # + sanitize(), + GenStats, generate_reply -> (String, GenStats),
                  #   build_chat_body() (testable), + summarize(); think:false + knobs in body
  context.rs      # NEW: layered context assembly + [Name]: speaker labeling + self-history cap
  orchestrator.rs # NEW: Dispatcher + RoomActor (coalesce/dedupe) + global inference Semaphore(1)
  router.rs       # handle() refactored into a per-turn TurnProcessor the actor calls; house-rules system prompt
  settings.rs     # NEW: Settings struct, bounds validation, EffectiveParams resolver (personality > global)
  metrics.rs      # NEW: in-memory ring buffer + aggregates (avg/p50/p95), TurnRecord, system+ollama snapshots
  summarizer.rs   # NEW (Phase 3): periodic per-room summary sweep
  store.rs        # + settings row get/set, + room_summaries get/upsert
  config.rs       # + seed defaults (keep_alive, ollama_timeout_secs, repeat_penalty, ...) 
  personalities.rs# + optional overrides (num_predict, num_ctx, temperature, top_p, repeat_penalty)
  web/
    mod.rs        # + routes for /settings, /health, /api/metrics
    handlers.rs   # + settings GET/POST (validated), health page, metrics JSON
    metrics_view  # (in handlers) system/ollama/llm/orchestration rendering
  main.rs         # wire Dispatcher to signal rx; seed settings; spawn summarizer (Phase 3)
migrations/
  0002_settings.sql        # settings single-row table
  0003_room_summaries.sql  # room_summaries table (Phase 3)
templates/
  settings.html, health.html, base.html (nav tabs)
personalities/
  default.toml, sage.toml, toxic.toml   # voice-only rewrites (house rules move to code)
tests/
  orchestrator_it.rs, context_*, settings_*, sanitize_*, summarizer_it.rs, web_settings_it.rs
```

---

# PHASE 1 — Conversational core

### Task 1: Output sanitization

**Files:**
- Modify: `src/llm.rs` (add `sanitize`, unit tests)

**Interfaces:**
- Produces: `pub fn sanitize(raw: &str) -> String` — removes `<think>…</think>` blocks, removes whole-token `/no_think` and `/think` (not substrings inside words), trims surrounding whitespace, collapses the resulting blank gaps.

- [ ] **Step 1: Write failing tests** (append to `src/llm.rs` tests mod):

```rust
#[test]
fn sanitize_strips_think_block() {
    assert_eq!(sanitize("<think>reason</think>Hello"), "Hello");
    assert_eq!(sanitize("a <think>x\ny</think> b"), "a  b".trim());
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
```

- [ ] **Step 2: Run to verify fail** — `cargo test llm::sanitize` → FAIL (`sanitize` not found). Capture output.

- [ ] **Step 3: Implement** (top of `src/llm.rs`):

```rust
/// Strip qwen3 thinking artifacts / control tokens from model output before it is
/// stored or sent (prevents the /no_think self-reinforcement + echo loop).
pub fn sanitize(raw: &str) -> String {
    // remove <think>...</think> (non-greedy, across newlines)
    let mut s = String::with_capacity(raw.len());
    let bytes = raw;
    let mut rest = bytes;
    loop {
        match rest.find("<think>") {
            Some(start) => {
                s.push_str(&rest[..start]);
                match rest[start..].find("</think>") {
                    Some(end) => { rest = &rest[start + end + "</think>".len()..]; }
                    None => { rest = ""; break; }
                }
            }
            None => { s.push_str(rest); break; }
        }
    }
    // remove whole-token /no_think and /think
    let cleaned: Vec<&str> = s
        .split_whitespace()
        .filter(|tok| *tok != "/no_think" && *tok != "/think")
        .collect();
    cleaned.join(" ").trim().to_string()
}
```

Note: `split_whitespace().join(" ")` also collapses the blank left by a removed block; the `a <think>..</think> b` test expects collapsed single spacing — assert against `"a b"`. Adjust that test's expected value to `"a b"` before Step 4.

- [ ] **Step 4: Run to verify pass** — `cargo test llm::sanitize` → PASS. `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 5: Commit** — `git commit -m "feat: sanitize LLM output (strip think blocks + control tokens)"`

### Task 2: GenStats + generate_reply returns stats

**Files:**
- Modify: `src/llm.rs` (ChatRequest unchanged for now; add GenStats, change trait + OllamaClient + MockLlm)
- Modify: `src/router.rs` (call site), `src/repl.rs` (call site if it prints reply)

**Interfaces:**
- Produces:
  - `pub struct GenStats { pub prompt_tokens: u32, pub reply_tokens: u32, pub total_ms: u64, pub eval_ms: u64 }` (derive Debug, Clone, Default)
  - `LlmBackend::generate_reply(&self, req: ChatRequest) -> anyhow::Result<(String, GenStats)>` (was `-> anyhow::Result<String>`); the returned text is already `sanitize()`d by the impl.
  - `MockLlm.generate_reply` returns `(self.reply.clone(), GenStats::default())`.

- [ ] **Step 1: Update the trait + MockLlm + OllamaClient** in `src/llm.rs`:
  - Change trait method signature to return `(String, GenStats)`.
  - `OllamaClient::chat` already returns the content string; add a sibling `chat_with_stats(model, msgs, opts) -> anyhow::Result<(String, GenStats)>` that parses Ollama's response JSON fields `prompt_eval_count`, `eval_count`, `total_duration` (ns), `eval_duration` (ns) into `GenStats { prompt_tokens, reply_tokens, total_ms: total_duration/1_000_000, eval_ms: eval_duration/1_000_000 }`, and returns `(sanitize(&content), stats)`.
  - `generate_reply` calls `chat_with_stats`.
  - `MockLlm::generate_reply` returns `Ok((self.reply.clone(), GenStats::default()))`.

```rust
#[derive(Debug, Clone, Default)]
pub struct GenStats { pub prompt_tokens: u32, pub reply_tokens: u32, pub total_ms: u64, pub eval_ms: u64 }
```

- [ ] **Step 2: Update call sites** — `src/router.rs` `handle`: `let (reply, _stats) = self.llm.generate_reply(...).await?;` (stats used in Task 10). `src/repl.rs`: destructure `(reply, _)`. Existing `tests/router_it.rs` MockLlm construction unchanged (fields same); assertions on reply text unchanged.

- [ ] **Step 3: Build + test** — `cargo test` → all pass (router_it/web_it unaffected; MockLlm text still returned). `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 4: Commit** — `git commit -m "feat: generate_reply returns GenStats (real Ollama token/timing counters), sanitized"`

### Task 3: LLM request knobs (testable body builder)

**Files:**
- Modify: `src/llm.rs`

**Interfaces:**
- Consumes: `ChatRequest` (extended in this task).
- Produces:
  - `ChatRequest` gains fields: `repeat_penalty: f32, repeat_last_n: u32, num_predict: i32, keep_alive: String`.
  - `pub(crate) fn build_chat_body(model, msgs: &[serde_json::Value], opts_extra: &ChatRequest) -> serde_json::Value` — builds the `/api/chat` body with `think:false`, `keep_alive`, and `options` containing `temperature, top_p, num_ctx, repeat_penalty, repeat_last_n, num_predict`.

- [ ] **Step 1: Write failing test** (llm tests mod):

```rust
#[test]
fn chat_body_has_think_false_and_knobs() {
    let req = ChatRequest { model:"qwen3:8b".into(), system:"s".into(), turns:vec![],
        temperature:0.7, top_p:0.9, num_ctx:8192, repeat_penalty:1.3, repeat_last_n:256,
        num_predict:512, keep_alive:"30m".into() };
    let msgs = vec![serde_json::json!({"role":"system","content":"s"})];
    let body = build_chat_body(&req.model, &msgs, &req);
    assert_eq!(body["think"], serde_json::json!(false));
    assert_eq!(body["keep_alive"], serde_json::json!("30m"));
    assert_eq!(body["options"]["num_predict"], serde_json::json!(512));
    assert_eq!(body["options"]["repeat_penalty"], serde_json::json!(1.3));
    assert_eq!(body["options"]["repeat_last_n"], serde_json::json!(256));
    assert_eq!(body["stream"], serde_json::json!(false));
}
```

- [ ] **Step 2: Run to verify fail** — `cargo test llm::chat_body` → FAIL. Capture.

- [ ] **Step 3: Implement** — add the fields to `ChatRequest`; implement `build_chat_body`; refactor `chat`/`chat_with_stats` and `generate_reply` to use it. `relevance_check` keeps its own small `num_predict:40` body (may also route through a variant of `build_chat_body` with num_predict override) and `think:false`. Update every `ChatRequest { .. }` literal (router, tests) to include the new fields.

- [ ] **Step 4: Run to verify pass** — `cargo test llm::` → PASS. `cargo clippy ... -D warnings`.

- [ ] **Step 5: Commit** — `git commit -m "feat: configurable Ollama knobs (keep_alive, repeat_penalty, num_predict) via testable body builder"`

### Task 4: settings table + Store accessors

**Files:**
- Create: `migrations/0002_settings.sql`
- Modify: `src/store.rs` (accessors + tests)

**Interfaces:**
- Produces:
  - `pub struct SettingsRow { keep_alive: String, ollama_timeout_secs: i64, repeat_penalty: f64, repeat_last_n: i64, num_predict: i64, num_ctx: i64, default_temperature: f64, default_top_p: f64, summary_enabled: bool, summary_interval_hours: i64 }`
  - `async fn get_settings(&self) -> anyhow::Result<SettingsRow>` (row id=1)
  - `async fn upsert_settings(&self, s: &SettingsRow) -> anyhow::Result<()>`
  - `async fn settings_exists(&self) -> anyhow::Result<bool>`

- [ ] **Step 1: Create migration** `migrations/0002_settings.sql`:

```sql
CREATE TABLE settings (
    id                     INTEGER PRIMARY KEY CHECK (id = 1),
    keep_alive             TEXT NOT NULL,
    ollama_timeout_secs    INTEGER NOT NULL,
    repeat_penalty         REAL NOT NULL,
    repeat_last_n          INTEGER NOT NULL,
    num_predict            INTEGER NOT NULL,
    num_ctx                INTEGER NOT NULL,
    default_temperature    REAL NOT NULL,
    default_top_p          REAL NOT NULL,
    summary_enabled        INTEGER NOT NULL,
    summary_interval_hours INTEGER NOT NULL
);
```

- [ ] **Step 2: Write failing test** (store tests):

```rust
#[tokio::test]
async fn settings_roundtrip() {
    let s = mem().await;
    assert!(!s.settings_exists().await.unwrap());
    let row = SettingsRow { keep_alive:"30m".into(), ollama_timeout_secs:300, repeat_penalty:1.3,
        repeat_last_n:256, num_predict:512, num_ctx:8192, default_temperature:0.7, default_top_p:0.9,
        summary_enabled:true, summary_interval_hours:6 };
    s.upsert_settings(&row).await.unwrap();
    assert!(s.settings_exists().await.unwrap());
    let got = s.get_settings().await.unwrap();
    assert_eq!(got.num_predict, 512);
    assert!(got.summary_enabled);
    let mut row2 = got.clone(); row2.num_predict = 1024;
    s.upsert_settings(&row2).await.unwrap();
    assert_eq!(s.get_settings().await.unwrap().num_predict, 1024);
}
```

- [ ] **Step 3: Run to verify fail** — `cargo test store::settings_roundtrip` → FAIL. Capture.

- [ ] **Step 4: Implement** `SettingsRow` (derive Clone, Debug) + the three methods in `src/store.rs`, using runtime `query()` with an `INSERT ... ON CONFLICT(id) DO UPDATE` upsert on `id=1`, and `bool` mapped to/from INTEGER.

- [ ] **Step 5: Run to verify pass** — `cargo test store::settings` → PASS. clippy.

- [ ] **Step 6: Commit** — `git commit -m "feat: settings table + typed Store accessors"`

### Task 5: Personality optional overrides

**Files:**
- Modify: `src/personalities.rs` (struct + tests)

**Interfaces:**
- Produces: `Personality` gains `pub num_predict: Option<i64>, pub num_ctx_override: Option<u32>, pub temperature_override: Option<f32>, pub top_p_override: Option<f32>, pub repeat_penalty: Option<f32>` — all `#[serde(default)]`. (Keep existing `temperature`/`top_p`/`num_ctx` fields as-is for back-compat; the new `*_override` optionals are what Task 6 prefers. To avoid confusion, name them distinctly as above.)

- [ ] **Step 1: Write failing test** (personalities tests) — a TOML with `num_predict = 800` and `repeat_penalty = 1.5` parses into `Some(800)` / `Some(1.5)`, and a TOML without them yields `None`.

```rust
#[test]
fn personality_optional_overrides_parse() {
    let d = tempfile::tempdir().unwrap();
    let body = "label=\"X\"\nsystem_prompt=\"hi\"\nmodel=\"qwen3:8b\"\ntemperature=0.5\ntop_p=0.9\nnum_ctx=8192\nnum_predict=800\nrepeat_penalty=1.5\n[proactive]\nrelevance_threshold=0.7\ncooldown_secs=60\nmax_per_hour=5\n";
    std::fs::write(d.path().join("default.toml"), body).unwrap();
    let p = Personalities::load_dir(d.path()).unwrap();
    let d0 = p.get("default").unwrap();
    assert_eq!(d0.num_predict, Some(800));
    assert_eq!(d0.repeat_penalty, Some(1.5));
}
```

- [ ] **Step 2: Run to verify fail** → FAIL (field missing). Capture.
- [ ] **Step 3: Implement** — add the `#[serde(default)]` optional fields to `Personality`.
- [ ] **Step 4: Run to verify pass** → PASS. clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: optional per-personality LLM param overrides"`

### Task 6: EffectiveParams resolver (settings.rs)

**Files:**
- Create: `src/settings.rs`
- Modify: `src/lib.rs` (`pub mod settings;`)

**Interfaces:**
- Consumes: `store::SettingsRow`, `personalities::Personality`.
- Produces:
  - `pub struct EffectiveParams { pub num_predict: i32, pub num_ctx: u32, pub temperature: f32, pub top_p: f32, pub repeat_penalty: f32, pub repeat_last_n: u32, pub keep_alive: String }`
  - `pub fn resolve(global: &SettingsRow, p: &Personality) -> EffectiveParams` — per field: personality override if `Some`, else global.
  - `pub fn validate(row: &SettingsRow) -> Result<(), String>` — bounds per Global Constraints; returns human-readable error naming the offending field.

- [ ] **Step 1: Write failing tests** (`src/settings.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SettingsRow;
    use crate::personalities::Personality;
    fn global() -> SettingsRow { SettingsRow { keep_alive:"30m".into(), ollama_timeout_secs:300,
        repeat_penalty:1.3, repeat_last_n:256, num_predict:512, num_ctx:8192,
        default_temperature:0.7, default_top_p:0.9, summary_enabled:true, summary_interval_hours:6 } }
    fn pers(np: Option<i64>, rp: Option<f32>) -> Personality { /* construct with overrides, other fields defaulted */ unimplemented!() }

    #[test]
    fn override_beats_global() {
        let p = pers(Some(800), Some(1.5));
        let e = resolve(&global(), &p);
        assert_eq!(e.num_predict, 800);
        assert_eq!(e.repeat_penalty, 1.5);
        assert_eq!(e.num_ctx, 8192); // not overridden -> global
    }
    #[test]
    fn validate_rejects_out_of_range() {
        let mut g = global(); g.num_predict = 99999;
        assert!(validate(&g).is_err());
        g.num_predict = 512; g.repeat_penalty = 5.0;
        assert!(validate(&g).is_err());
        assert!(validate(&global()).is_ok());
    }
}
```

(Implementer: replace the `pers()` `unimplemented!()` with a real `Personality` literal — construct via the struct's public fields; all non-override fields get any valid value.)

- [ ] **Step 2: Run to verify fail** → FAIL. Capture.
- [ ] **Step 3: Implement** `EffectiveParams`, `resolve`, `validate`. Add `pub mod settings;` to `lib.rs`.
- [ ] **Step 4: Run to verify pass** → PASS. clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: EffectiveParams resolver + settings bounds validation"`

### Task 7: Layered context + speaker labeling (context.rs)

**Files:**
- Create: `src/context.rs`
- Modify: `src/lib.rs` (`pub mod context;`)

**Interfaces:**
- Consumes: `types::{StoredMessage, ChatTurn, Role}`.
- Produces:
  - `pub const MAX_WINDOW_MSGS: usize = 18;` `pub const MAX_BOT_TURNS: usize = 2;`
  - `pub fn build_context_turns(recent_chrono: &[StoredMessage], bot_id: &str) -> Vec<ChatTurn>` — takes chronological recent messages, keeps at most the last `MAX_WINDOW_MSGS`, but drops all-but-the-last `MAX_BOT_TURNS` of the bot's own messages (a message is the bot's if `sender_id == bot_id || role == Assistant`); labels each retained message: human → `ChatTurn { role: User, name: Some("[Name]"), content }` where the rendered content will be `[Name]: text` (see llm turn_json), bot → `ChatTurn { role: Assistant, name: Some("[you]"), content }`. Preserves chronological order.
  - `pub fn speaker_note() -> &'static str` — the system line explaining `[Name]:` labels.

- [ ] **Step 1: Write failing tests** (`src/context.rs`):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StoredMessage, Role};
    fn m(id: i64, sender: &str, role: Role, body: &str) -> StoredMessage {
        StoredMessage { id, room_id:"r".into(), sender_id:sender.into(), sender_name:Some(sender.into()),
            role, body:body.into(), ts:id, personality:None, is_mention:false }
    }
    #[test]
    fn caps_bot_self_history_to_two() {
        // 5 human + 5 interleaved bot messages
        let mut v = vec![];
        for i in 0..5 { v.push(m(i*2, "Alice", Role::User, "hi")); v.push(m(i*2+1, "+bot", Role::Assistant, "yo")); }
        let turns = build_context_turns(&v, "+bot");
        let bot_turns = turns.iter().filter(|t| t.role == Role::Assistant).count();
        assert_eq!(bot_turns, 2, "bot self-history must be capped at 2");
    }
    #[test]
    fn labels_humans_with_name() {
        let v = vec![ m(1, "Alice", Role::User, "hey") ];
        let turns = build_context_turns(&v, "+bot");
        assert_eq!(turns[0].role, Role::User);
        assert_eq!(turns[0].name.as_deref(), Some("[Alice]"));
    }
    #[test]
    fn respects_max_window() {
        let v: Vec<_> = (0..40).map(|i| m(i, "Alice", Role::User, "x")).collect();
        assert_eq!(build_context_turns(&v, "+bot").len(), MAX_WINDOW_MSGS);
    }
}
```

- [ ] **Step 2: Run to verify fail** → FAIL. Capture.
- [ ] **Step 3: Implement** `build_context_turns` (drop excess bot turns first, then take last `MAX_WINDOW_MSGS`, preserving order), `speaker_note()`. Add module to `lib.rs`.
- [ ] **Step 4: Run to verify pass** → PASS. clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: layered context assembly with capped bot self-history + speaker labels"`

### Task 8: House rules + system_prompt rewrite

**Files:**
- Modify: `src/router.rs` (`system_prompt`, tests)

**Interfaces:**
- Produces: `pub fn system_prompt(personality: &Personality, room: &Room) -> String` — now returns `format!("{HOUSE_RULES}\n\n{}\n\n{}\n{}", personality.system_prompt.trim(), room_context_line(room), context::speaker_note())` where `HOUSE_RULES` is a `const &str` containing the Section 10 conduct rules.

- [ ] **Step 1: Write failing test** (router tests):

```rust
#[test]
fn system_prompt_includes_house_rules_and_speaker_note() {
    let p = /* build a Personality with system_prompt "You are Sage." */;
    let r = room(ReplyMode::Addressed, true);
    let s = system_prompt(&p, &r);
    assert!(s.contains("useful") && s.contains("mandatory")); // answer-mandatory rule
    assert!(s.contains("square brackets")); // speaker note
    assert!(s.contains("You are Sage.")); // character preserved
}
```

- [ ] **Step 2: Run to verify fail** → FAIL. Capture.
- [ ] **Step 3: Implement** — add `const HOUSE_RULES: &str = "..."` (the eight Section 10 bullets as prose), keep `room_context_line`, compose with `context::speaker_note()`.
- [ ] **Step 4: Run to verify pass** → PASS. clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: shared house-rules conduct prepended to every personality system prompt"`

### Task 9: metrics.rs (store + aggregates)

**Files:**
- Create: `src/metrics.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces:
  - `pub struct TurnRecord { pub room_id: String, pub ts: i64, pub decision: &'static str, pub wait_ms: u64, pub gen_ms: u64, pub prompt_tokens: u32, pub reply_tokens: u32, pub outcome: &'static str }`
  - `pub struct Metrics { inner: Mutex<Ring> }` with `pub fn new() -> Arc<Metrics>`, `pub fn record(&self, r: TurnRecord)`, `pub fn snapshot(&self) -> MetricsSnapshot`.
  - `pub struct MetricsSnapshot { pub replies: u64, pub errors: u64, pub timeouts: u64, pub avg_gen_ms: u64, pub p50_gen_ms: u64, pub p95_gen_ms: u64, pub avg_prompt_tokens: u32, pub avg_reply_tokens: u32, pub avg_tokens_per_sec: f32, pub recent: Vec<TurnRecord> }` (last-hour aggregates over the ring).

- [ ] **Step 1: Write failing tests** — record synthetic turns, assert `snapshot()` computes count, avg, p50, p95 correctly over a known set (e.g. gen_ms [100,200,300,400] → avg 250, p50 ~200/300, p95 ~400); ring caps at 500.

- [ ] **Step 2: Run to verify fail** → FAIL. Capture.
- [ ] **Step 3: Implement** ring buffer (`VecDeque` cap 500), aggregates (percentiles via sorted copy of last-hour gen_ms), counters. `pub mod metrics;`.
- [ ] **Step 4: Run to verify pass** → PASS. clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: in-memory metrics store with response-time aggregates"`

### Task 10: TurnProcessor (router.handle refactor) — consumes settings/context/metrics

**Files:**
- Modify: `src/router.rs`
- Modify: `tests/router_it.rs`

**Interfaces:**
- Consumes: `Store`, `Personalities`, `LlmBackend` ((String,GenStats)), `SignalTransport`, `settings::{resolve,EffectiveParams}`, `context::build_context_turns`, `metrics::{Metrics,TurnRecord}`.
- Produces: `Router` gains `metrics: Arc<Metrics>` field and takes it in `new(...)`. `handle` now: self-sender guard; ensure_room; record incoming; load global settings (`store.get_settings`); resolve `EffectiveParams` (personality > global); build context via `build_context_turns` + coalesced burst; `decide`; proactive gate (uses effective params in ChatRequest); generate; (text already sanitized); on non-dry-run record assistant + send; **emit a `TurnRecord` to metrics** (wait_ms passed in from the actor, gen_ms from GenStats.total_ms, tokens from GenStats, decision/outcome strings). Returns `Ok(Option<String>)` as before.

- [ ] **Step 1: Update `tests/router_it.rs`** — `Router::new` now takes `Arc<Metrics>`; construct `Metrics::new()` in each test. Add a test: after a direct-message reply, `metrics.snapshot().replies == 1`. Existing behavior assertions unchanged.

- [ ] **Step 2: Run to verify fail** — `cargo test --test router_it` → FAIL (arity/API mismatch). Capture.

- [ ] **Step 3: Implement** the refactor: thread `EffectiveParams` into the `ChatRequest` (num_predict, num_ctx, temperature, top_p, repeat_penalty, repeat_last_n, keep_alive); replace `build_turns` usage with `context::build_context_turns`; build `ChatRequest.turns` = context turns + the current burst turn(s); destructure `(reply, stats)`; record metrics. Keep `decide`/proactive/rate-limit logic (including the proactive-when-addressed + dry-run-no-rate-limit fixes already present). The **coalesced burst** (a `Vec<IncomingMessage>`) is passed into a new `handle_burst(&self, msgs: Vec<IncomingMessage>, wait_ms: u64) -> Result<Option<String>>`; `handle` becomes `handle_burst(vec![msg], 0)` for back-compat with existing tests.

- [ ] **Step 4: Run to verify pass** — `cargo test` → all pass. clippy.

- [ ] **Step 5: Commit** — `git commit -m "feat: TurnProcessor consumes effective params, layered context, records metrics; handle_burst for coalescing"`

### Task 10-note: `ChatRequest` construction

All `ChatRequest { .. }` literals must now include `repeat_penalty, repeat_last_n, num_predict, keep_alive` (from Task 3). Ensure router builds them from `EffectiveParams`; ensure relevance_check path is unaffected (it builds its own body).

### Task 11: orchestrator.rs — Dispatcher + RoomActor + global permit

**Files:**
- Create: `src/orchestrator.rs`
- Create: `tests/orchestrator_it.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Arc<Router>` (its `handle_burst`), `types::IncomingMessage`.
- Produces:
  - `pub struct Dispatcher { /* rooms: Mutex<HashMap<String, mpsc::Sender<IncomingMessage>>>, router, permit: Arc<Semaphore>, idle: Duration */ }`
  - `Dispatcher::new(router: Arc<Router>) -> Arc<Dispatcher>` (global `Semaphore::new(1)`).
  - `async fn Dispatcher::dispatch(&self, msg: IncomingMessage)` — routes to (or spawns) the room's actor via mpsc; dedupe of exact-duplicate ts happens in the actor.
  - RoomActor internal: loop over the mpsc receiver with `recv_many`/drain to coalesce: on first message, collect it + any immediately-available queued messages (drain the channel without blocking), dedupe by ts against `last_processed_ts`, then acquire the global permit and call `router.handle_burst(batch, wait_ms)`. While the permit is held / handle runs, further messages queue in the mpsc and are drained on the next loop iteration (coalesced).
  - For testability, the burst-processing dependency is the `Router` behind a small trait `TurnHandler { async fn handle_burst(&self, msgs, wait_ms) -> Result<Option<String>> }` implemented by `Router`, so tests inject a counting fake.

- [ ] **Step 1: Write failing integration tests** (`tests/orchestrator_it.rs`), using a fake `TurnHandler` that records each batch (with a small sleep to simulate generation):

```rust
// pseudocode shape — real impl uses the exported types
// 1. burst_coalesces: send 5 messages fast to one room -> fake records exactly ONE batch of (<=5) messages (or a small number of batches, but far fewer than 5), never 5 separate single-message turns.
// 2. dedupe: send the same message (same ts) twice -> processed once.
// 3. two_rooms_independent: a slow turn in room A does not delay room B's turn starting.
// 4. global_serialization: instrument the fake to assert concurrent-in-flight count never exceeds 1 across rooms.
```

Write these as real `#[tokio::test]`s with `tokio::time::sleep` and `AtomicUsize` counters in the fake handler; assert the coalescing/dedupe/serialization invariants above with concrete numbers.

- [ ] **Step 2: Run to verify fail** — `cargo test --test orchestrator_it` → FAIL (Dispatcher not found). Capture.

- [ ] **Step 3: Implement** Dispatcher + RoomActor + `TurnHandler` trait + global `Semaphore(1)`. RoomActor drains the channel with `while let Ok(m) = rx.try_recv()` after the first `rx.recv().await` to coalesce; tracks `last_processed_ts`; measures `wait_ms` = now − oldest-buffered-arrival. Idle reaping: `tokio::select!` on `rx.recv()` with a `tokio::time::sleep(idle)`; on idle timeout the actor removes itself from the map and exits. Add `pub mod orchestrator;`.

- [ ] **Step 4: Run to verify pass** — `cargo test --test orchestrator_it` + full `cargo test` → PASS. clippy.

- [ ] **Step 5: Commit** — `git commit -m "feat: orchestrator (per-room actors, burst coalescing, dedupe, global inference permit)"`

### Task 12: config seed defaults + configurable Ollama timeout

**Files:**
- Modify: `src/config.rs`, `src/llm.rs` (OllamaClient timeout param)

**Interfaces:**
- Produces:
  - `AppConfig` gains `#[serde(default = "...")]` seed fields matching `SettingsRow` (keep_alive, ollama_timeout_secs, repeat_penalty, repeat_last_n, num_predict, num_ctx, default_temperature, default_top_p, summary_enabled, summary_interval_hours), each with a default fn per Global Constraints. Add `fn seed_settings_row(&self) -> store::SettingsRow`.
  - `OllamaClient::new(base_url, timeout_secs: u64)` — timeout now a parameter (was hard-coded 120).

- [ ] **Step 1: Write failing test** — `AppConfig` parsed from a minimal TOML (existing required fields) yields the default seed values (e.g. `cfg.num_predict == 512`, `cfg.ollama_timeout_secs == 300`); `seed_settings_row()` maps them. Also update the `OllamaClient::new` callers.

- [ ] **Step 2: Run to verify fail** → FAIL. Capture.
- [ ] **Step 3: Implement** the serde defaults + `seed_settings_row` + `OllamaClient::new(base_url, timeout_secs)`; update `deploy/config.example.toml` with the new (optional) keys documented as comments.
- [ ] **Step 4: Run to verify pass** → PASS. clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: config seed defaults for settings + configurable Ollama timeout"`

### Task 13: main.rs wiring (Dispatcher + settings seed)

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Implement** — after `Store::connect`, if `!store.settings_exists()` then `store.upsert_settings(&cfg.seed_settings_row())`. Build `OllamaClient::new(cfg.ollama_url, store.get_settings().await?.ollama_timeout_secs as u64)`. Build `Metrics::new()`, pass to `Router::new`. Build `Arc<Router>`, then `let dispatcher = Dispatcher::new(router);`. Replace the `while let Some(msg) = rx.recv().await { tokio::spawn(...) }` loop with `while let Some(msg) = rx.recv().await { dispatcher.dispatch(msg).await; }` (dispatch is cheap: routes to actor). Keep the SSE publish (move it into the actor/turn path or publish on dispatch). Keep web + hot-reload spawns.

- [ ] **Step 2: Build** — `cargo build`; `cargo test`; `cargo clippy --all-targets -- -D warnings`. (No unit test for main; integration covered by orchestrator_it + router_it.)

- [ ] **Step 3: Commit** — `git commit -m "feat: wire Dispatcher + settings seed + metrics into main"`

### Task 14: Personality TOML rewrites (voice-only + toxic examples)

**Files:**
- Modify: `personalities/default.toml`, `personalities/sage.toml`, `personalities/toxic.toml`

- [ ] **Step 1: Rewrite** each `system_prompt` to **character/voice only** (house rules now come from code). For `toxic.toml`: keep the abrasive voice, but **remove the "Dude, you just said 〈quote〉" examples** and replace with 3–4 short, varied, **answer-first** rude examples that do NOT quote the incoming message (e.g. a one-line answer with a jab). Keep the safety limits (no slurs/protected-class attacks, humor-not-threats). Leave the `[proactive]` + param fields intact; optionally set `num_predict`/`repeat_penalty` overrides for `toxic`.

- [ ] **Step 2: Validate** — `python3 -c 'import tomllib,glob; [tomllib.load(open(f,"rb")) for f in glob.glob("personalities/*.toml")]'` parses all; `cargo test personalities::` still green.

- [ ] **Step 3: Commit** — `git commit -m "refactor: personality prompts to voice-only; rewrite toxic examples (no whole-message quoting)"`

**Phase 1 ships here:** the bot coalesces bursts, one generation at a time, sanitized output, capped self-history, speaker labels, house rules, configurable knobs (from DB settings seeded by config), metrics recorded. Deploy + on-host verify before Phase 2.

---

# PHASE 2 — Web control plane (options screen + health dashboard)

### Task 15: Settings screen (GET/POST /settings)

**Files:**
- Modify: `src/web/mod.rs` (route + AppState gains `store` already present), `src/web/handlers.rs`
- Create: `templates/settings.html`
- Create/Modify: `tests/web_settings_it.rs`

**Interfaces:**
- Consumes: `Store::{get_settings, upsert_settings}`, `settings::validate`.
- Produces: `GET /settings` (auth) renders the form from `get_settings()`; `POST /settings` (auth) parses the form into a `SettingsRow`, runs `settings::validate`, on Ok `upsert_settings` + redirect back with a success flash, on Err re-render with the error message.

- [ ] **Step 1: Write failing test** (`tests/web_settings_it.rs`, authenticated via the existing login helper): GET `/settings` → 200 and body contains `num_predict`; POST `/settings` with `num_predict=1024` (valid) → 303 and `store.get_settings().num_predict == 1024`; POST with `num_predict=99999` (invalid) → 200 (re-render) and store unchanged.

- [ ] **Step 2: Run to verify fail** → FAIL (route missing). Capture.
- [ ] **Step 3: Implement** the handlers + `settings.html` (a labeled form; each field has a `?` with a `title=`/tooltip using the plain-language help text from spec §2b; number inputs with `min`/`max` matching bounds). Add routes behind the auth layer.
- [ ] **Step 4: Run to verify pass** → PASS. clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: web options screen for LLM settings with validation + help tooltips"`

### Task 16: System + Ollama snapshots

**Files:**
- Create: `src/metrics.rs` additions — `pub fn system_snapshot() -> SystemSnapshot` (parse `/proc/meminfo`, `/proc/loadavg`, self RSS from `/proc/self/status` VmRSS), and in `src/llm.rs` add `OllamaClient::ps(&self) -> anyhow::Result<OllamaPs>` (GET `/api/ps`).

**Interfaces:**
- Produces: `SystemSnapshot { mem_total_kb, mem_available_kb, load1: f32, rss_kb: u64, uptime_secs: u64 }`; `OllamaPs { models: Vec<{name, size_bytes, expires_at: Option<String>}> }`.

- [ ] **Step 1: Write failing test** — `parse_meminfo(sample: &str) -> (u64,u64)` pure helper tested on a sample `/proc/meminfo` string (extract MemTotal + MemAvailable); `parse_loadavg("0.50 0.40 ...") -> 0.50`.
- [ ] **Step 2: Run to verify fail** → FAIL. Capture.
- [ ] **Step 3: Implement** pure parsers + `system_snapshot()` (reads the files, calls parsers) + `OllamaClient::ps` (reqwest GET, serde). Uptime from a process-start `Instant` captured in `Metrics::new`.
- [ ] **Step 4: Run to verify pass** → PASS. clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: system (/proc) + Ollama (/api/ps) snapshots for health view"`

### Task 17: Health page + /api/metrics + orchestration gauges

**Files:**
- Modify: `src/web/mod.rs`, `src/web/handlers.rs`, `src/orchestrator.rs` (expose `Dispatcher::gauges()`), `src/web/mod.rs` AppState gains `Arc<Metrics>` + `Arc<Dispatcher>` (or a gauges provider)
- Create: `templates/health.html`

**Interfaces:**
- Consumes: `Metrics::snapshot`, `metrics::system_snapshot`, `OllamaClient::ps`, `Dispatcher::gauges() -> DispatcherGauges { in_flight: Option<{room, elapsed_ms}>, per_room_buffered: Vec<(String,usize)>, active_actors: usize }`.
- Produces: `GET /health` (auth) HTML page; `GET /api/metrics` (auth) JSON combining metrics snapshot + system snapshot + ollama ps + dispatcher gauges. Page polls `/api/metrics` every 5s with a small vanilla-JS fetch and updates stat tiles.

- [ ] **Step 1: Write failing test** — `GET /api/metrics` (authed) → 200, `content-type: application/json`, body parses and contains keys `system`, `ollama`, `llm`, `orchestration`. (Use `MockLlm`-less path: metrics snapshot works without Ollama; `ps` failure degrades to `ollama: {reachable:false}`.)
- [ ] **Step 2: Run to verify fail** → FAIL. Capture.
- [ ] **Step 3: Implement** `Dispatcher::gauges()` (read the rooms map + an `AtomicU64`/shared in-flight marker), the JSON handler (degrade gracefully if `/api/ps` errors), and `health.html` (stat tiles + RAM bar + a small response-time list; built per `frontend-design` guidance). Wire AppState to carry `Metrics` + a gauges provider.
- [ ] **Step 4: Run to verify pass** → PASS. clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: health dashboard (system/ollama/llm/orchestration) + /api/metrics"`

### Task 18: Nav + polish

**Files:** `templates/base.html`, `templates/rooms.html`, `templates/health.html`, `templates/settings.html`

- [ ] **Step 1:** Add a nav bar to `base.html` linking Rooms / Health / Settings; ensure consistent styling; theme-aware; confirm all pages render under auth. `cargo test` green.
- [ ] **Step 2: Commit** — `git commit -m "feat: dashboard nav across rooms/health/settings"`

**Phase 2 ships here:** operator can view live health + edit LLM settings from the browser. Deploy + verify.

---

# PHASE 3 — Long-term summary memory

### Task 19: room_summaries table + Store accessors

**Files:**
- Create: `migrations/0003_room_summaries.sql`
- Modify: `src/store.rs`

**Interfaces:**
- Produces: `pub struct RoomSummary { pub summary: String, pub covered_through_ts: i64 }`; `async fn get_summary(&self, room_id) -> Result<Option<RoomSummary>>`; `async fn upsert_summary(&self, room_id, summary: &str, covered_through_ts: i64) -> Result<()>`; `async fn rooms_with_new_messages_since_summary(&self) -> Result<Vec<String>>` (rooms whose max(messages.ts) > coalesced summary covered_through_ts, or that have no summary yet but have messages).

- [ ] **Step 1: Create migration**:

```sql
CREATE TABLE room_summaries (
    room_id            TEXT PRIMARY KEY REFERENCES rooms(room_id),
    summary            TEXT NOT NULL DEFAULT '',
    covered_through_ts INTEGER NOT NULL DEFAULT 0,
    updated_at         INTEGER NOT NULL
);
```

- [ ] **Step 2: Write failing test** — upsert + get round-trip; `rooms_with_new_messages_since_summary` returns a room after messages recorded, empty after summary covers them.
- [ ] **Step 3: Run to verify fail** → FAIL. Capture.
- [ ] **Step 4: Implement** accessors (runtime `query()`).
- [ ] **Step 5: Run to verify pass** → PASS. clippy.
- [ ] **Step 6: Commit** — `git commit -m "feat: room_summaries table + accessors"`

### Task 20: LlmBackend::summarize

**Files:** `src/llm.rs`

**Interfaces:**
- Produces: `LlmBackend::summarize(&self, model: &str, prior: &str, transcript: &str) -> anyhow::Result<String>` — one chat call with a neutral summarization system prompt (spec §9), `think:false`, modest `num_predict` (~300); output `sanitize()`d. `MockLlm::summarize` returns a canned string.

- [ ] **Step 1: Add trait method + impls.** `SUMMARY_SYS` const. Build via `build_chat_body`. (No unit test for the network path; a MockLlm test in Task 21.)
- [ ] **Step 2: Build + test** → green. clippy.
- [ ] **Step 3: Commit** — `git commit -m "feat: LlmBackend::summarize with neutral summary prompt"`

### Task 21: summarizer.rs sweep

**Files:**
- Create: `src/summarizer.rs`, `tests/summarizer_it.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Store`, `Arc<Personalities>` (for model), `Arc<dyn LlmBackend>`, global `Arc<Semaphore>` (shared with orchestrator), settings (`summary_enabled`, `summary_interval_hours`).
- Produces: `pub async fn run_sweep_once(store, llm, model, permit) -> Result<usize>` (returns rooms summarized); `pub fn spawn(store, llm, model, permit, interval)` (loops `run_sweep_once` every interval).

- [ ] **Step 1: Write failing integration test** (`tests/summarizer_it.rs`) with `MockLlm` + in-memory store: record messages in room "G"; `run_sweep_once` summarizes it (get_summary now Some, covered_through_ts advanced); a second immediate `run_sweep_once` with no new messages summarizes 0 rooms (activity-gating).

- [ ] **Step 2: Run to verify fail** → FAIL. Capture.
- [ ] **Step 3: Implement** `run_sweep_once`: for each `rooms_with_new_messages_since_summary`, load prior summary + messages since `covered_through_ts` (bounded), acquire the permit, `llm.summarize`, `upsert_summary` with new `covered_through_ts` = max ts folded in. `spawn` loops with `tokio::time::interval`. Add module.
- [ ] **Step 4: Run to verify pass** → PASS. clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: per-room summarization sweep (activity-gated, permit-guarded)"`

### Task 22: Inject summary into context + wire sweep

**Files:** `src/context.rs`, `src/router.rs`, `src/main.rs`

**Interfaces:**
- Produces: `context::summary_block(summary: &str) -> Option<String>` (token-capped ~250 → a system note) ; router prepends it to the system prompt / as a leading system turn when the room has a summary; `main.rs` spawns `summarizer::spawn(...)` using the shared global semaphore and `get_settings().summary_interval_hours` (skip if `!summary_enabled`).

- [ ] **Step 1: Write failing test** (context tests) — `summary_block("")` is `None`; a long summary is truncated to the cap; router test: a room with a stored summary includes the "Earlier in this room:" note in the system prompt (assert via a MockLlm that captures the system string, or via a unit test of the assembly fn).
- [ ] **Step 2: Run to verify fail** → FAIL. Capture.
- [ ] **Step 3: Implement** `summary_block` + router injection (load `store.get_summary(room)` in `handle_burst`, prepend block) + `main.rs` sweep spawn sharing the orchestrator's `Arc<Semaphore>`.
- [ ] **Step 4: Run to verify pass** → PASS; full `cargo test`; clippy.
- [ ] **Step 5: Commit** — `git commit -m "feat: inject per-room summary as long-term context layer; wire sweep"`

**Phase 3 ships here.**

---

## Deployment (all phases)

Build/deploy exactly as established: `cargo build --release` locally → `rsync` binary + changed sources to `bot@bot.local:~/signal-bot` → operator runs `sudo install -m755 ~/signal-bot/target/release/signal-bot /opt/signal-bot/signal-bot` (needs their sudo; not in the scoped grant) → controller `sudo systemctl restart signal-bot` (in scope) → verify `ss -tln | grep 8443`, `journalctl -u signal-bot`, and a live group test (bursts→one reply, no dupes, no `/no_think`, correct speaker attribution, factual questions answered, dashboard sane). Migrations run automatically on connect (sqlx `migrate!`).

## Notes for the executor

- Tasks 1–14 (Phase 1) are the priority — they resolve every tester finding and are fully unit/integration-testable with mocks (no live Ollama/Signal). Do these first; Phase 1 is independently deployable.
- The orchestrator (Task 11) and the router refactor (Task 10) are the highest-risk tasks — use a standard/most-capable model for those; the pure-logic tasks (sanitize, context, settings, metrics) are cheap-model transcription-plus-testing.
- `handle_burst` is the seam between orchestrator and router; keep its signature stable (`Vec<IncomingMessage>, wait_ms: u64`).
- Run `cargo clippy --all-targets -- -D warnings` before every commit (the tree is currently clippy-clean; keep it so).
