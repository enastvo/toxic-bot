# Signal Bot — Conversation Quality, Orchestration & Observability — Design Spec

**Date:** 2026-09-20
**Status:** Approved design, pre-implementation
**Author:** estefan.nastvogel@icloud.com (with Claude)
**Builds on:** `docs/superpowers/specs/2026-09-19-signal-bot-design.md` (the shipped bot)

## 1. Context

After 24 hours of live group testing (the "Bit Blasters" room, `toxic` personality on `qwen3:8b`), the bot's *conversational architecture* — not its toxicity — proved to be the problem. Human testers reverse-engineered it as a "manual with a mouth": it collapses into a deterministic insult template, confuses who said what, replies to stale messages, duplicates output, leaks a `/no_think` control token, and often insults instead of answering. Code + live-log review confirmed the mechanisms and traced them to concrete defects. Separately, the operator wants owner-level control over LLM limits and live visibility (RAM, response time, context) into what the bare-metal box is doing, plus long-term per-room memory.

This spec addresses all of it, implemented in three phases.

### Confirmed root causes (from code + `journalctl` review)

- **Orchestration:** `main.rs` spawns one `tokio` task per incoming message with **no per-room serialization, coalescing, cancellation, or dedupe**. Bursts launch concurrent `handle()` calls racing on the same room → out-of-order + duplicate replies, and **concurrent qwen3 generations on a 7 GB CPU box** (a perf killer). The log shows the same reply re-emitted at 11:48, 12:05, 12:05, 12:06, 12:07.
- **`/no_think` contamination:** not from users, not from Ollama (probed: `think:false` returns clean output). qwen3 emitted `/no_think` **once**, it was **stored as an assistant message and fed back**, and the **echo-loop** (bot quoting the whole prior message) carried and nested it forever. Root = **no output sanitization + storing/feeding back the bot's own output + quoting whole messages**.
- **Template collapse:** the context window feeds up to **60 recent messages, heavy with the bot's own near-identical replies** (`router.rs:105`); the model imitates its last outputs over the system prompt. The `toxic` prompt's examples literally teach "Dude, you just said 〈quote〉", fueling the echo loop.
- **Speaker confusion:** `build_turns` maps all humans to `role=user` with a weak `"Name: text"` prefix and the bot to nameless `role=assistant` — speakers blur.
- **"Rooms not replying while 1:1 works":** most likely (a) a group in `addressed` mode not @-mentioned (silent by design), or (b) **large group prompts exceeding the hard-coded 120 s client timeout** → error → no reply, while short 1:1 prompts finish fast. Not a token limit; raising `num_ctx` would worsen (b).

## 2. Goals

- Eliminate template collapse, speaker confusion, stale/duplicate replies, and control-token leakage.
- Make the bot answer questions accurately (persona is flavor, not a substitute).
- One in-flight generation per room, coalescing bursts into a single reply; at most one qwen3 generation process-wide.
- Owner-controllable LLM limits (reply length, context size, timeout, repetition, keep-alive), editable from the web UI with plain-language help.
- A web health dashboard: system RAM/load, Ollama status, real response-time/token metrics, per-room orchestration state.
- Per-room long-term memory via periodic summarization injected as context.
- Keep `qwen3:8b`; get speed from resident model + single-flight + caps, not a model swap.

### Non-goals

- Changing the Signal transport, auth, TLS, or deployment model.
- Multiple models / model auto-selection (per-personality `model` already exists; unused here).
- Persisting metrics history across restarts (in-memory live metrics only in v1).
- Web-editing of personality prompts (personalities stay TOML + hot-reload; only global LLM settings are web-editable).

## 3. Phasing

One spec, three implementation phases (each independently shippable and testable):

- **Phase 1 — Conversational core:** orchestration (per-room actors + global inference permit), output sanitization, layered context + speaker labeling, LLM sampling/timeout/keep-alive with higher configurable defaults, DB-backed settings store, personality-prompt rewrite (shared house rules + character-only prompts).
- **Phase 2 — Web control plane:** options screen (edit the LLM settings, plain-language help icons) + health dashboard (system + Ollama + LLM + orchestration metrics).
- **Phase 3 — Long-term memory:** per-room periodic summarization → DB → injected context layer.

## 4. Orchestration (Phase 1)

New module `src/orchestrator.rs`.

```
signal rx ─► Dispatcher ─(route by room_id)─► per-room RoomActor task
                │  map: room_id → mpsc::Sender      owns: pending buffer, last_processed_ts
   generation ─►│  Global inference Semaphore(1)    loop: recv → dedupe by ts → buffer;
   (all LLM)    └───────────────────────────────    when idle & buffer non-empty:
                                                       drain buffer → one coalesced turn →
                                                       acquire permit → (relevance?) → generate →
                                                       sanitize → record → send → release
```

- **Dispatcher:** consumes the `SignalCli` receiver; for each message, finds or spawns the `RoomActor` for `msg.room_id` and forwards over an `mpsc`. Replaces the per-message `tokio::spawn` loop in `main.rs`.
- **RoomActor** (one per active room): single loop; never two turns at once for its room. Messages arriving during a turn accumulate in a pending buffer; when the turn completes, the whole buffer is drained and treated as **one coalesced turn**. Dedupe by Signal message timestamp so an envelope can't fire twice.
- **Global inference `Semaphore(1)`:** every LLM call (relevance gate, reply, and Phase 3 summaries) acquires this single permit → **only one qwen3 generation runs process-wide** (memory/perf fix). Rooms coalesce independently; generations never overlap.
- **Coalesced-turn semantics:** the buffered burst is appended to the recent-window context; the **newest** message determines the trigger (mention/proactive) and the reply addresses it while seeing the whole burst — avoids N replies to N messages.
- **Idle reaping:** a RoomActor idle (empty buffer, no traffic) ~30 min exits to free memory; respawns on next message.
- **Metrics hook:** each turn emits a record (room, wait, generation time, token counts, decision, outcome) to the Phase 2 metrics store.
- `router.rs` keeps its per-turn logic (decision, context build, LLM, send) as the body the actor invokes; `main.rs` shrinks to wiring the dispatcher.

## 5. Output sanitization (Phase 1)

A `sanitize()` step in the `llm` layer runs on **every** model reply **before it is stored or sent**:
- strip `<think>…</think>` blocks (defensive),
- strip standalone control directives `/no_think` and `/think` (whole-token, not substrings),
- collapse leading/trailing whitespace.

Sanitizing **before persisting** ensures a control token never enters stored history and thus can never be fed back and echoed — killing the self-reinforcement at the source. `think:false` remains set on all calls.

## 6. LLM knobs + DB-backed settings (Phase 1)

Runtime-tunable LLM settings move into a **`settings` table in the bot's SQLite DB** (writable by the service user). `config.toml` becomes the **bootstrap**: on first run the DB is seeded from it; thereafter the DB is source of truth. This enables live web edits without restart or root.

Global settings (seeded defaults, DB-stored, web-editable in Phase 2):
- `keep_alive` (default `"30m"`) — keep qwen3 resident between calls.
- `ollama_timeout_secs` (default `300`) — raised from the hard-coded 120 s.
- `repeat_penalty` (default `1.3`), `repeat_last_n` (default `256`).
- `num_predict` (default `512`) — reply-length cap; today reply generation sets none.
- `num_ctx` (default `8192`).
- `default_temperature`, `default_top_p`.
- (Phase 3) `summary_enabled` (default true), `summary_interval_hours` (default 6).

**Precedence for a generation param:** per-personality TOML override (if set) → DB global setting. `config.toml` only seeds the DB.

Per-personality TOML gains optional overrides: `num_predict`, `num_ctx`, `temperature`, `top_p`, `repeat_penalty` (all fall back to the DB global).

Ollama request changes: add top-level `keep_alive`; add `repeat_penalty`, `repeat_last_n`, `num_predict` to `options`. `generate_reply` returns `(text, GenStats)` capturing Ollama's `prompt_eval_count`, `eval_count`, and durations for metrics.

## 7. Layered context + speaker labeling (Phase 1)

New module `src/context.rs` assembles model input in three layers:
1. **Long-term summary** (Phase 3): per-room running summary as a system note ("Earlier in this room: …"), token-capped (~250). Empty slot until Phase 3.
2. **Trimmed recent window:** last ~15–20 messages, but the **bot's own replies capped at the most recent ~2** (older bot turns dropped from what's fed back) — starves the self-imitation attractor; human messages kept.
3. **Current burst:** the coalesced just-arrived message(s).

**Speaker labeling:** every line rendered with a fixed, explicit `[Name]:` prefix inside the content; human turns are `role=user` prefixed `[DisplayName]:`, the bot's few retained turns are `role=assistant` labeled as itself (`[you, as <Label>]:`). A system line states: "Each message is prefixed with who said it in square brackets. Multiple people are talking; never attribute one person's words to another." Replaces the current weak `"Name: text"` scheme in `build_turns`.

The window/self-history cap makes the settings key `num_ctx` govern the model's context size while the *number of retained messages* is bounded separately (count-based trim), so raising `num_ctx` no longer means feeding 60 self-authored messages back.

## 8. Health dashboard + options screen (Phase 2)

**Metrics store:** shared in-memory ring buffer (~last 500 turns) + running counters (live state, not history; resets on restart). Each turn records room, decision (replied / proactive-skipped / rate-limited / error / timeout), queue wait, generation time, and — from Ollama's `/api/chat` response counters (real, not estimated) — `prompt_eval_count` (context tokens), `eval_count` (reply tokens), durations → tokens/sec.

**Web additions** (behind the existing admin login), organized as tabs in the admin area:
- **`GET /health`** page + **`GET /api/metrics`** JSON (polled ~5 s):
  - *System:* RAM used/available/total (`/proc/meminfo`), load average (`/proc/loadavg`), bot RSS, uptime.
  - *Ollama:* loaded model + resident status + `keep_alive` expiry (`GET /api/ps`), model size, reachability.
  - *LLM performance:* avg / p50 / p95 generation time (last hour), replies sent, errors + timeouts, avg context tokens, avg reply tokens, avg tokens/sec.
  - *Orchestration:* current in-flight generation (room + elapsed), per-room buffered/queued counts, active RoomActor count.
- **`GET/POST /settings`** options screen: form for the Section 6 knobs, each with a **plain-language help tooltip** (`?` icon), with **bounds validation** (timeout 10–600 s, num_predict 16–4096, repeat_penalty 0.5–2.0, num_ctx 512–32768, etc.). Save updates the DB; router/LLM read current values per turn (cached), effective next message.

Built with the `frontend-design` guidance (stat tiles, a RAM bar, a small response-time trend) so it reads as a clean status page, not a wall of numbers.

## 9. Summarization / long-term memory (Phase 3)

New module `src/summarizer.rs`. New table:
```sql
CREATE TABLE room_summaries (
    room_id            TEXT PRIMARY KEY REFERENCES rooms(room_id),
    summary            TEXT NOT NULL DEFAULT '',
    covered_through_ts INTEGER NOT NULL DEFAULT 0,
    updated_at         INTEGER NOT NULL
);
```

- **Cadence:** background sweep on a configurable interval (default 6 h; `summary_interval_hours`, `summary_enabled` in settings). **Activity-gated:** only rooms with new messages since `covered_through_ts` are (re)summarized; idle rooms cost nothing.
- **Generation:** per due room, load prior summary + messages since `covered_through_ts`, one LLM call with a **dedicated neutral summarization prompt** (not the room's personality): "Update the running summary of this group chat — ongoing topics, running jokes, notable facts, who tends to say what. Merge with the prior summary. Under ~200 words." Output sanitized and stored; `covered_through_ts` advanced.
- **Scheduling politeness:** runs through the same global `Semaphore(1)` and **defers to live traffic** (a room with buffered messages is summarized after its live turn, never blocking a reply).
- **Injection:** layer 1 in `context.rs`, token-capped (~250) so it can't bloat the prompt or become imitation fodder.

## 10. Personality-prompt rewrite (Phase 1)

Split **conduct** (shared, structural) from **character** (per-personality). `system_prompt()` assembly:
```
[shared HOUSE RULES] + [personality system_prompt = voice only] + [room context + speaker-label note]
```

**Shared house rules** (in code, prepended to every personality — one place, can't drift):
- A useful/accurate answer is **mandatory**; persona flavor is optional and never a substitute. Answer factual questions (may stay in character).
- Never use a stock template; never reuse a recent opener/closer/insult/joke; never start two replies the same way.
- Don't quote or paraphrase the whole message you're replying to.
- Attribute correctly using the `[Name]:` prefixes; never put one person's words in another's mouth.
- Match length; short in → short out; don't insult every message.
- Acknowledge/fire back at a good joke rather than mechanically denying.
- Don't "correct" a user's spelling/caps/emoji when referring to it.
- If asked for something impossible over Signal (e.g. post an image), say so briefly instead of pretending.

**Per-personality `system_prompt` = character only.** `toxic.toml` keeps the rude voice but its **examples are rewritten** to remove the whole-message-quoting pattern and show short, varied, **answer-first** rude replies. `default`/`sage` keep their voices and inherit the house rules. All hot-reloadable; house rules live in code.

## 11. Data model & interface changes (summary)

- New table `settings` — a **single row** (`id INTEGER PRIMARY KEY CHECK (id = 1)`) with one **typed column per knob** (keep_alive TEXT, ollama_timeout_secs INTEGER, repeat_penalty REAL, repeat_last_n INTEGER, num_predict INTEGER, num_ctx INTEGER, default_temperature REAL, default_top_p REAL, summary_enabled INTEGER, summary_interval_hours INTEGER), seeded from `config.toml` on first run. Typed columns (not free-form key/value) so reads and bounds-validation are straightforward.
- New table `room_summaries` (Phase 3).
- `Store`: `get_settings`/`set_setting` (or typed accessors); `room_summaries` get/upsert; `recent` gains a variant or the trim happens in `context`.
- `Personality` TOML: optional `num_predict`, `num_ctx`, `temperature`, `top_p`, `repeat_penalty`.
- `LlmBackend::generate_reply` → returns `(String, GenStats)`; new `summarize()` method; `chat()` body adds `keep_alive`, `repeat_penalty`, `repeat_last_n`, `num_predict`; output `sanitize()`.
- New modules: `orchestrator.rs`, `context.rs`, `summarizer.rs`, plus web `settings`/`health` handlers + templates + a `metrics` store.
- `main.rs`: wire Dispatcher to the signal receiver; spawn summarizer sweep (Phase 3).

## 12. Testing

- **Orchestration:** unit/integration tests with mocks — a burst of N messages to one room yields exactly one coalesced reply; two rooms don't block each other; the global permit serializes generations (assert no overlap via a counting mock LLM); dedupe by ts drops repeats.
- **Sanitization:** unit tests — `<think>…</think>`, leading/trailing `/no_think`, `/think` are stripped; real words containing "think" are untouched; sanitized text is what's stored/sent.
- **Context/speaker:** unit tests — trimmed window caps bot self-turns at 2; `[Name]:` labels present and correct; summary layer injected when present, absent when empty.
- **Settings:** round-trip DB settings; bounds validation rejects out-of-range; precedence (personality override > DB global) resolves correctly.
- **Metrics:** GenStats parsed from a canned Ollama response; aggregates (avg/p50/p95) computed correctly over a synthetic ring buffer.
- **Web:** `/settings` GET renders + POST validates/persists (via `tower::ServiceExt::oneshot`, auth cookie); `/api/metrics` returns JSON behind auth.
- **Summarizer:** activity-gating (idle room skipped); merge advances `covered_through_ts`; output sanitized; defers to live traffic.
- **On-host verification:** after deploy, confirm in a live group that bursts get one reply, no duplicates, no `/no_think`, speakers attributed correctly, factual questions answered; dashboard shows sane RAM/timings; settings edits take effect.

## 13. Risks & mitigations

- **8B on CPU still limited:** house rules + repeat penalty + capped self-history reduce but can't fully eliminate an 8B model's tendency to fall into patterns; the dashboard + tunable knobs let the operator adapt. Model swap remains an escape hatch (per-personality `model` exists) but is out of scope.
- **Coalescing can merge distinct asks:** a burst answered as one reply may under-serve a multi-question burst; acceptable for group chat and matches the chosen behavior.
- **Summary latency/bloat:** capped length + activity-gating + deferral to live traffic keep cost low; toggle to disable.
- **Settings misconfiguration:** bounds validation + safe defaults; a bad value can't brick the bot (clamped/rejected).
- **Metrics reset on restart:** acceptable for v1 (live state); DB persistence is a noted future add.
