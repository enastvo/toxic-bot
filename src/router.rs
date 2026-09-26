//! Per-turn message handling: record incoming messages, decide whether to
//! reply (reply mode + proactive relevance gate + rate limit), compose the
//! system prompt and context window, generate (optionally via the bounded
//! tool loop), then persist and send the reply.

use crate::llm::{ChatRequest, LlmBackend};
use crate::metrics::{Metrics, TurnRecord};
use crate::personalities::{Personalities, Personality};
use crate::signal::SignalTransport;
use crate::store::{NewMessage, Store};
use crate::types::{IncomingMessage, ReplyMode, Role, Room};
use crate::web::SseEvent;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision { Reply, Proactive, Silent }

pub fn decide(room: &Room, msg: &IncomingMessage) -> Decision {
    if !room.is_group { return Decision::Reply; }
    match room.reply_mode {
        ReplyMode::Always => Decision::Reply,
        ReplyMode::Addressed => if msg.is_mention || msg.quoted_msg.is_some() { Decision::Reply } else { Decision::Silent },
        // Proactive: always reply when directly addressed (mention/quote), and
        // otherwise defer to the relevance gate to decide whether to chime in.
        ReplyMode::Proactive => {
            if msg.is_mention || msg.quoted_msg.is_some() {
                Decision::Reply
            } else {
                Decision::Proactive
            }
        }
    }
}

/// The metrics `decision` string for a routing decision (ruling T9-a).
fn decision_str(d: Decision) -> &'static str {
    match d {
        Decision::Reply => "reply",
        Decision::Proactive => "proactive",
        Decision::Silent => "silent",
    }
}

/// Strip a leading copy of the persona's OWN name that some models (notably the
/// abliterated Qwen2.5 builds) prepend to replies, e.g. "Liberal Activist: hi",
/// "[Liberal Activist] hi" or "[Liberal Activist]: hi" -> "hi". Only the persona's
/// own label at the very start is removed (case-insensitive), so ordinary text is
/// untouched. Complements `sanitize()`, which only catches the "[label]:" form.
fn strip_own_label(reply: &str, label: &str) -> String {
    let t = reply.trim_start();
    let label = label.trim();
    if label.is_empty() {
        return t.to_string();
    }
    for cand in [format!("[{label}]:"), format!("[{label}]"), format!("{label}:")] {
        if let Some(head) = t.get(..cand.len()) {
            if head.eq_ignore_ascii_case(&cand) {
                return t[cand.len()..].trim_start().to_string();
            }
        }
    }
    t.to_string()
}

/// A human-readable note of the current LOCAL system date/time, prepended to each
/// reply's system prompt so any model/persona knows "now" without a tool call.
/// Read fresh every turn; `chrono::Local` handles the host timezone and DST.
fn current_time_note() -> String {
    format!(
        "Current date and time: {}.",
        chrono::Local::now().format("%A, %B %-d, %Y, %-I:%M %p %Z")
    )
}

/// A short, single-line preview of a (possibly long, multi-line) tool result for
/// logging — so `journalctl` shows what a tool actually returned without dumping
/// whole search payloads.
fn preview(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

/// True if an error (anywhere in its cause chain) looks like a generation timeout.
fn error_is_timeout(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        let s = c.to_string().to_lowercase();
        s.contains("timeout") || s.contains("timed out")
    })
}

#[derive(Default)]
pub struct RateLimiter { rooms: Mutex<HashMap<String, RoomRate>> }
#[derive(Default)]
struct RoomRate { last: u64, hits: Vec<u64> }

impl RateLimiter {
    /// Whether a proactive reply would be allowed now, WITHOUT recording one.
    /// Used to skip the relevance model call when the answer would be
    /// discarded anyway.
    pub fn would_allow(&self, room: &str, cooldown_secs: u64, max_per_hour: u32, now: u64) -> bool {
        let g = self.rooms.lock().unwrap();
        let Some(e) = g.get(room) else { return true };
        if e.last != 0 && now.saturating_sub(e.last) < cooldown_secs { return false; }
        let recent = e.hits.iter().filter(|t| now.saturating_sub(**t) < 3600).count();
        (recent as u32) < max_per_hour
    }

    /// Returns true if a proactive reply is allowed now, and records it.
    pub fn allow(&self, room: &str, cooldown_secs: u64, max_per_hour: u32, now: u64) -> bool {
        let mut g = self.rooms.lock().unwrap();
        let e = g.entry(room.to_string()).or_default();
        e.hits.retain(|t| now.saturating_sub(*t) < 3600);
        if e.last != 0 && now.saturating_sub(e.last) < cooldown_secs { return false; }
        if e.hits.len() as u32 >= max_per_hour { return false; }
        e.last = now; e.hits.push(now);
        true
    }
}

/// An operator `!steer` command parsed from a message body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerCommand {
    /// Set this room's steering directive to the given text.
    Set(String),
    /// Clear this room's steering directive.
    Clear,
    /// Reply with the room's current steering directive.
    Show,
}

/// Parse a `!steer` operator command from a message body, or `None` if it isn't one.
/// `!steer <text>` sets, `!steer` / `!steer clear` clears, `!steer?` shows. The
/// `!steer` prefix is case-insensitive and must be followed by end, whitespace, or `?`.
pub fn parse_steer_command(body: &str) -> Option<SteerCommand> {
    let t = body.trim();
    if t.len() < 6 || !t[..6].eq_ignore_ascii_case("!steer") {
        return None;
    }
    let rest = &t[6..];
    match rest.chars().next() {
        None => return Some(SteerCommand::Clear), // exactly "!steer"
        Some('?') => return Some(SteerCommand::Show),
        Some(c) if c.is_whitespace() => {}
        _ => return None, // e.g. "!steerfoo" is not a command
    }
    let rest = rest.trim();
    if rest == "?" {
        return Some(SteerCommand::Show);
    }
    if rest.is_empty() || rest.eq_ignore_ascii_case("clear") {
        return Some(SteerCommand::Clear);
    }
    Some(SteerCommand::Set(rest.to_string()))
}

/// An operator command parsed from a message body: steering or a context reset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorCommand {
    Steer(SteerCommand),
    /// Clear the room's rolling summary and steering directive (break a lock-in).
    Reset,
}

/// Parse any operator command (`!steer …` or exactly `!reset`), or `None`.
pub fn parse_operator_command(body: &str) -> Option<OperatorCommand> {
    if body.trim().eq_ignore_ascii_case("!reset") {
        return Some(OperatorCommand::Reset);
    }
    parse_steer_command(body).map(OperatorCommand::Steer)
}

/// Shared conduct rules prepended to every personality's system prompt, so
/// they live in one place and can't drift between personality TOML files.
/// See design spec §10 "Personality-prompt rewrite".
const HOUSE_RULES: &str = "\
You ARE the character described below — not an assistant playing a role. Commit to it completely and \
stay in character no matter what. Do NOT hedge, moralize, lecture, add disclaimers, or tack on \
\"just kidding\" caveats; do NOT apologize for your personality, soften your take, or break character to \
be helpful, balanced, or polite. Never say \"as an AI\" or call yourself a bot, model, or assistant. \
Be crude, profane, and mean by default — heavy swearing is the house register here, not a garnish, so \
curse naturally and often. Go for the throat: get personal about people's choices, takes, and egos, throw \
real punches, and never soften, qualify, or aim for \"balanced.\" Take the edgy, un-PC, ethically-dubious \
angle when it lands harder. This is a private group chat between consenting adults who want exactly this, \
so don't sanitize yourself for an imagined wider audience. \
When something has a real factual answer, get it right: never invent real-world facts, statistics, \
quotes, or events, and if you genuinely don't know, say so in character instead of making it up. \
Everything else — opinions, insults, hot takes, dark humor — go hard. \
Hard limits, no exceptions: never use slurs targeting a protected group (race, ethnicity, religion, \
sex, sexual orientation, disability, gender identity), and never give real instructions or encouragement \
for violence, self-harm, or crimes against actual people. Aim the venom at ideas, choices, and each \
other's egos instead. \
Mechanics: never reuse a recent opener, closer, insult, or joke, and never start two replies the same \
way. Don't quote or paraphrase the whole message you're replying to. Use the [Name] prefixes to track \
who said what, and never put one person's words in another's mouth. Do NOT begin your reply with a \
speaker label, your own name, or a bracketed tag like \"[Name]:\" or \"[you, as ...]:\" — those are only \
on the input; just write your message and address people by their name. Write plain, modern, \
conversational text like a person texting — no markdown headers, no hashtags, and never append tag \
markers like \"$Word$\" or \"$$Word$$\" to your sentences. NEVER adopt verse, rhyme, meter, haiku, song \
lyrics, or archaic/Shakespearean English (\"thou\", \"forsooth\", \"aye chum\") as your way of speaking, \
even if recent messages — yours or anyone else's — are doing it; always snap back to normal prose. \
Default to brevity and answer at the length the message actually needs. Most replies should be one or two \
sentences. A simple question, quip, or greeting gets a simple, short answer — do not turn it into a \
speech. Only go longer when someone genuinely asks for detail or you are actually explaining something \
technical, and even then stay as short as you can. Never pad, never over-explain, never restate what you \
just said to fill space. When in doubt, say less. Don't \"correct\" anyone's spelling, capitalization, or \
emoji. If asked for something impossible over Signal (e.g. posting an image), say so briefly instead of \
pretending to do it.";

/// The room-specific context line describing where the conversation is
/// happening (group vs. direct message), inserted between the personality's
/// voice and the speaker-label note.
fn room_context_line(room: &Room) -> String {
    if room.is_group {
        let name = room.display_name.as_deref().unwrap_or("a group");
        format!("You are in a Signal group named \"{}\". Multiple people talk here; each user message is prefixed with the speaker's name.", name)
    } else {
        "You are in a direct message conversation with one person.".to_string()
    }
}

pub fn system_prompt(personality: &Personality, room: &Room) -> String {
    format!(
        "{HOUSE_RULES}\n\n{}\n\n{}\n{}",
        personality.system_prompt.trim(),
        room_context_line(room),
        crate::context::speaker_note()
    )
}

/// Compose the full system prompt, prepending the room's long-term summary
/// note (if any/non-empty) ahead of the personality's system prompt.
pub(crate) fn compose_system(personality: &Personality, room: &Room, summary: Option<&str>, steer: Option<&str>) -> String {
    let base = system_prompt(personality, room);
    let with_summary = match summary.and_then(crate::context::summary_block) {
        Some(note) => format!("{note}\n\n{base}"),
        None => base,
    };
    match steer_block(steer) {
        Some(block) => format!("{with_summary}\n\n{block}"),
        None => with_summary,
    }
}

/// Format the operator steering directive as a high-priority trailing block, or
/// `None` when unset/blank. Placed last so it's the most recent instruction; it
/// overrides stylistic preferences but explicitly not the hard-harm floor.
fn steer_block(steer: Option<&str>) -> Option<String> {
    let s = steer.map(str::trim).filter(|s| !s.is_empty())?;
    Some(format!(
        "OPERATOR DIRECTIVE for this room — follow it exactly. It overrides the tone, style, \
         format, and length guidance above, but NOT the hard limits: {s}"
    ))
}

pub struct Router {
    store: Store,
    personalities: Arc<Personalities>,
    llm: Arc<dyn LlmBackend>,
    signal: Arc<dyn SignalTransport>,
    bot_id: String,
    dry_run: bool,
    rl: RateLimiter,
    metrics: Arc<Metrics>,
    sse: Option<tokio::sync::broadcast::Sender<crate::web::SseEvent>>,
    /// Web-search provider for the `web_search` tool. `None` = web search
    /// unavailable (no API key configured), regardless of the DB toggle.
    search: Option<Arc<dyn crate::search::SearchProvider>>,
    /// Sender ids allowed to issue `!steer` operator commands.
    operators: Vec<String>,
}

fn now_secs() -> u64 { time::OffsetDateTime::now_utc().unix_timestamp() as u64 }
fn now_ms() -> i64 { (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64 }

impl Router {
    #[allow(clippy::too_many_arguments)]
    pub fn new(store: Store, personalities: Arc<Personalities>, llm: Arc<dyn LlmBackend>,
               signal: Arc<dyn SignalTransport>, bot_id: String, dry_run: bool, metrics: Arc<Metrics>,
               sse: Option<tokio::sync::broadcast::Sender<crate::web::SseEvent>>,
               search: Option<Arc<dyn crate::search::SearchProvider>>,
               operators: Vec<String>) -> Self {
        Self { store, personalities, llm, signal, bot_id, dry_run, rl: RateLimiter::default(), metrics, sse, search, operators }
    }

    /// True if `sender_id` is a configured operator allowed to run `!steer`.
    fn is_operator(&self, sender_id: &str) -> bool {
        self.operators.iter().any(|o| o == sender_id)
    }

    /// Bounded tool-call loop. Offers the model tool schemas; when it requests a
    /// call, we validate + run it (in `tools::execute`) and feed the result back
    /// as DATA, then continue — capped at `settings.max_tool_rounds` rounds
    /// (each round is another generation). On the final round tools are withheld
    /// so the model must produce text. Returns the final reply + aggregated stats.
    async fn run_tool_loop(
        &self,
        req: &ChatRequest,
        settings: &crate::store::SettingsRow,
        room_id: &str,
        extra_domains: &[String],
    ) -> anyhow::Result<(String, crate::llm::GenStats)> {
        let mut messages = crate::llm::base_messages(&req.system, &req.turns);
        let web_available = settings.web_search_enabled && self.search.is_some();
        // Global whitelist plus this personality's own extra domains (persona
        // sources don't leak into other personalities' searches).
        let whitelist = crate::tools::merged_whitelist(&settings.search_whitelist, extra_domains);
        let schemas = crate::tools::tool_schemas(web_available);
        let max_rounds = settings.max_tool_rounds.max(0) as usize;

        let mut total_ms = 0u64;
        let mut rounds = 0usize;
        loop {
            let offer = if rounds < max_rounds { schemas.as_slice() } else { &[] };
            let step = self.llm.chat_step(messages.clone(), offer, req).await?;
            total_ms += step.stats.total_ms;
            // Stop when the model is done calling tools, or once the round
            // budget is spent. Tools are withheld on the final round, but a
            // model can still emit tool calls; ignoring them here guarantees
            // the loop terminates (it holds the global inference permit).
            if step.tool_calls.is_empty() || rounds >= max_rounds {
                if !step.tool_calls.is_empty() {
                    tracing::warn!(room=%room_id, calls=step.tool_calls.len(), "ignoring tool calls past max_tool_rounds");
                }
                let mut stats = step.stats;
                stats.total_ms = total_ms;
                return Ok((step.content, stats));
            }
            // Record the assistant's tool-call request, then each result, so the
            // next round has the full context.
            let tc_json: Vec<serde_json::Value> = step.tool_calls.iter().map(|tc| {
                serde_json::json!({ "function": { "name": tc.name, "arguments": tc.arguments } })
            }).collect();
            messages.push(serde_json::json!({
                "role": "assistant", "content": step.content, "tool_calls": tc_json
            }));
            let ctx = crate::tools::ToolCtx {
                store: &self.store,
                room_id,
                search: self.search.as_deref(),
                whitelist: &whitelist,
            };
            for tc in &step.tool_calls {
                tracing::info!(room=%room_id, round=rounds, tool=%tc.name, args=%tc.arguments, "tool call");
                let result = crate::tools::execute(&tc.name, &tc.arguments, &ctx).await;
                tracing::info!(room=%room_id, tool=%tc.name, result_len=result.len(), result=%preview(&result, 240), "tool result");
                messages.push(serde_json::json!({ "role": "tool", "content": result }));
            }
            rounds += 1;
        }
    }

    /// Back-compat single-message entry point: a burst of one, with no wait.
    pub async fn handle(&self, msg: IncomingMessage) -> anyhow::Result<Option<String>> {
        self.handle_burst(vec![msg], 0).await
    }

    /// Process a coalesced burst of messages — all for the SAME room — as a
    /// single turn. `wait_ms` is how long the burst was allowed to accumulate
    /// before processing (recorded into metrics by the caller/orchestrator).
    pub async fn handle_burst(&self, msgs: Vec<IncomingMessage>, wait_ms: u64) -> anyhow::Result<Option<String>> {
        // Defense-in-depth: never process/echo our own messages. If the batch
        // is empty after filtering, store nothing and send nothing.
        let msgs: Vec<IncomingMessage> = msgs.into_iter().filter(|m| m.sender_id != self.bot_id).collect();
        if msgs.is_empty() {
            return Ok(None);
        }

        // Intercept operator `!steer` commands: apply them, send a short
        // confirmation, and drop them from the burst (never recorded into context,
        // never given a normal LLM reply). A non-operator's `!steer` falls through
        // as an ordinary message.
        let (commands, msgs): (Vec<IncomingMessage>, Vec<IncomingMessage>) = msgs
            .into_iter()
            .partition(|m| self.is_operator(&m.sender_id) && parse_operator_command(&m.body).is_some());
        if let Some(first) = commands.first() {
            let room_id = first.room_id.clone();
            let is_group = first.is_group;
            self.store.ensure_room(&room_id, first.sender_name.as_deref().filter(|_| !is_group), is_group).await?;
            let mut confirm = String::new();
            for m in &commands {
                match parse_operator_command(&m.body).expect("partition guarantees a command") {
                    OperatorCommand::Steer(SteerCommand::Set(text)) => {
                        self.store.set_steer(&room_id, Some(&text)).await?;
                        confirm = "🫡 steering set for this room.".into();
                    }
                    OperatorCommand::Steer(SteerCommand::Clear) => {
                        self.store.set_steer(&room_id, None).await?;
                        confirm = "🫡 steering cleared for this room.".into();
                    }
                    OperatorCommand::Steer(SteerCommand::Show) => {
                        confirm = match self.store.get_steer(&room_id).await? {
                            Some(s) => format!("current steering: {s}"),
                            None => "no steering set for this room.".into(),
                        };
                    }
                    OperatorCommand::Reset => {
                        // Blank the rolling summary and mark everything up to this
                        // command as already-covered, so the summarizer starts fresh
                        // from here instead of re-chewing (and re-priming) the backlog.
                        self.store.upsert_summary(&room_id, "", m.timestamp).await?;
                        // Also exclude everything up to now from the reply context
                        // window — this is what actually breaks a style lock-in, since
                        // the recent messages (not just the summary) drive it.
                        self.store.set_context_cutoff(&room_id, m.timestamp).await?;
                        self.store.set_steer(&room_id, None).await?;
                        confirm = "🧹 context reset for this room (summary + recent-context + steering cleared).".into();
                    }
                }
                tracing::info!(room=%room_id, cmd=%m.body, "operator command");
            }
            if !self.dry_run && !confirm.is_empty() {
                self.signal.send(&room_id, is_group, &confirm).await?;
            }
        }
        if msgs.is_empty() {
            return Ok(None);
        }

        let newest = msgs.last().unwrap();
        let room = self.store.ensure_room(
            &newest.room_id,
            newest.sender_name.as_deref().filter(|_| !newest.is_group),
            newest.is_group,
        ).await?;

        // Record every (non-self) incoming message in arrival order, and
        // publish it to the SSE stream (if configured) for live dashboard views.
        for m in &msgs {
            self.store.record_message(NewMessage {
                room_id: m.room_id.clone(), sender_id: m.sender_id.clone(), sender_name: m.sender_name.clone(),
                role: Role::User, body: m.body.clone(), ts: m.timestamp, personality: None, is_mention: m.is_mention,
            }).await?;
            if let Some(sse) = &self.sse {
                let sender = m.sender_name.clone().unwrap_or_else(|| m.sender_id.clone());
                let _ = sse.send(SseEvent { room_id: m.room_id.clone(), sender, body: m.body.clone() });
            }
        }

        let personality = self.personalities.get_or_default(room.personality.as_deref());

        // Global settings -> effective params (personality overrides global).
        let settings = self.store.get_settings().await?;
        let eff = crate::settings::resolve(&settings, &personality);

        // Ruling T10-c: prefer an addressed message as the decision trigger,
        // else fall back to the newest message in the burst.
        let trigger = msgs
            .iter()
            .rev()
            .find(|m| m.is_mention || m.quoted_msg.is_some())
            .unwrap_or_else(|| msgs.last().unwrap());
        let decision = decide(&room, trigger);
        tracing::debug!(room=%room.room_id, ?decision, mode=?room.reply_mode, "routing");
        if decision == Decision::Silent { return Ok(None); }

        // Build the layered context window. The just-recorded burst is already
        // the tail of `recent` — do NOT append burst turns a second time.
        let recent = self.store.recent(&room.room_id, 60).await?;
        // Drop anything at/before an operator context reset so a style lock-in
        // can't keep re-priming from the old message window.
        let cutoff = self.store.get_context_cutoff(&room.room_id).await?;
        let recent: Vec<_> = recent
            .into_iter()
            .filter(|m| cutoff.is_none_or(|c| m.ts > c))
            .collect();
        let turns = crate::context::build_context_turns(&recent, &self.bot_id);

        if decision == Decision::Proactive {
            // Cheap rate-limit check first: during a cooldown or once the
            // hourly cap is hit, skip the relevance model call entirely (it
            // holds the global inference permit and would be discarded).
            let pro = &personality.proactive;
            if !self.dry_run && !self.rl.would_allow(&room.room_id, pro.cooldown_secs, pro.max_per_hour, now_secs()) {
                tracing::debug!(room=%room.room_id, "proactive rate-limited (relevance check skipped)");
                self.metrics.record(TurnRecord {
                    room_id: room.room_id.clone(), ts: now_ms(), decision: "proactive", wait_ms,
                    gen_ms: 0, prompt_tokens: 0, reply_tokens: 0, outcome: "rate_limited",
                });
                return Ok(None);
            }
            let rel = self.llm.relevance_check(&personality.model, eff.num_ctx, turns.clone(), settings.ollama_timeout_secs.max(1) as u64).await?;
            tracing::debug!(room=%room.room_id, should=rel.should_reply, conf=rel.confidence, thr=personality.proactive.relevance_threshold, "relevance");
            if !rel.should_reply || rel.confidence < personality.proactive.relevance_threshold {
                self.metrics.record(TurnRecord {
                    room_id: room.room_id.clone(), ts: now_ms(), decision: "proactive", wait_ms,
                    gen_ms: 0, prompt_tokens: 0, reply_tokens: 0, outcome: "skipped",
                });
                return Ok(None);
            }
            if !self.dry_run && !self.rl.allow(&room.room_id, personality.proactive.cooldown_secs, personality.proactive.max_per_hour, now_secs()) {
                tracing::debug!(room=%room.room_id, "proactive rate-limited");
                self.metrics.record(TurnRecord {
                    room_id: room.room_id.clone(), ts: now_ms(), decision: "proactive", wait_ms,
                    gen_ms: 0, prompt_tokens: 0, reply_tokens: 0, outcome: "rate_limited",
                });
                return Ok(None);
            }
        }

        let dstr = decision_str(decision);

        let room_summary = self.store.get_summary(&room.room_id).await?;
        let steer = self.store.get_steer(&room.room_id).await?;
        let chatreq = ChatRequest {
            model: personality.model.clone(),
            system: format!(
                "{}\n\n{}",
                current_time_note(),
                compose_system(&personality, &room, room_summary.as_ref().map(|s| s.summary.as_str()), steer.as_deref())
            ),
            turns,
            temperature: eff.temperature,
            top_p: eff.top_p,
            num_ctx: eff.num_ctx,
            repeat_penalty: eff.repeat_penalty as f64,
            repeat_last_n: eff.repeat_last_n,
            num_predict: eff.num_predict,
            keep_alive: eff.keep_alive.clone(),
            ollama_timeout_secs: settings.ollama_timeout_secs.max(1) as u64,
        };
        // When tools are enabled, drive the bounded tool-call loop; otherwise a
        // single generation (unchanged behavior). Web-search domains for this
        // persona = its TOML `extra_search_domains` plus any dashboard-configured
        // domains from the DB (merged/deduped against the global whitelist inside
        // the loop).
        let gen = if settings.tools_enabled {
            let mut extra = personality.extra_search_domains.clone();
            extra.extend(self.store.get_persona_domains(&personality.name).await?);
            self.run_tool_loop(&chatreq, &settings, &room.room_id, &extra).await
        } else {
            self.llm.generate_reply(chatreq).await
        };

        let (reply, stats) = match gen {
            Ok(v) => v,
            Err(e) => {
                let outcome = if error_is_timeout(&e) { "timeout" } else { "error" };
                self.metrics.record(TurnRecord {
                    room_id: room.room_id.clone(), ts: now_ms(), decision: dstr, wait_ms,
                    gen_ms: 0, prompt_tokens: 0, reply_tokens: 0, outcome,
                });
                return Err(e);
            }
        };

        // Some models echo their persona label at the very start of the reply;
        // strip the persona's own name before storing/sending.
        let reply = strip_own_label(&reply, &personality.label);
        if reply.trim().is_empty() { return Ok(None); }

        if self.dry_run {
            tracing::info!(room=%room.room_id, %reply, "[dry-run] would send");
            self.metrics.record(TurnRecord {
                room_id: room.room_id.clone(), ts: now_ms(), decision: dstr, wait_ms,
                gen_ms: stats.total_ms, prompt_tokens: stats.prompt_tokens, reply_tokens: stats.reply_tokens,
                outcome: "dry_run",
            });
            return Ok(Some(reply));
        }

        // Send first, then record: a reply that never reached Signal must not
        // show up in history (dashboard, future context) as if it was said.
        if let Err(e) = self.signal.send(&room.room_id, room.is_group, &reply).await {
            self.metrics.record(TurnRecord {
                room_id: room.room_id.clone(), ts: now_ms(), decision: dstr, wait_ms,
                gen_ms: stats.total_ms, prompt_tokens: stats.prompt_tokens, reply_tokens: stats.reply_tokens,
                outcome: "error",
            });
            return Err(e);
        }
        self.store.record_message(NewMessage {
            room_id: room.room_id.clone(), sender_id: self.bot_id.clone(), sender_name: Some(personality.label.clone()),
            role: Role::Assistant, body: reply.clone(), ts: now_ms(),
            personality: Some(personality.name.clone()), is_mention: false,
        }).await?;
        if let Some(sse) = &self.sse {
            let _ = sse.send(SseEvent { room_id: room.room_id.clone(), sender: "bot".into(), body: reply.clone() });
        }

        self.metrics.record(TurnRecord {
            room_id: room.room_id.clone(), ts: now_ms(), decision: dstr, wait_ms,
            gen_ms: stats.total_ms, prompt_tokens: stats.prompt_tokens, reply_tokens: stats.reply_tokens,
            outcome: "sent",
        });
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests {
    use super::{Decision, RateLimiter, decide, system_prompt, compose_system};
    use crate::types::{Room, ReplyMode, IncomingMessage};
    use crate::personalities::{Personality, ProactiveConfig};

    fn room(mode: ReplyMode, is_group: bool) -> Room {
        Room { room_id: "r".into(), display_name: None, is_group, personality: None, reply_mode: mode }
    }
    fn msg(is_group: bool, mention: bool) -> IncomingMessage {
        IncomingMessage { room_id: "r".into(), sender_id: "u".into(), sender_name: None, body: "hi".into(),
            is_group, is_mention: mention, quoted_msg: None, timestamp: 0 }
    }

    #[test]
    fn direct_message_always_replies() {
        assert!(matches!(decide(&room(ReplyMode::Addressed, false), &msg(false, false)), Decision::Reply));
    }
    #[test]
    fn group_addressed_needs_mention() {
        assert!(matches!(decide(&room(ReplyMode::Addressed, true), &msg(true, false)), Decision::Silent));
        assert!(matches!(decide(&room(ReplyMode::Addressed, true), &msg(true, true)), Decision::Reply));
    }
    #[test]
    fn always_mode_replies_unconditionally() {
        assert!(matches!(decide(&room(ReplyMode::Always, true), &msg(true, false)), Decision::Reply));
    }
    #[test]
    fn proactive_mode_defers_to_gate() {
        // Unaddressed group message in proactive mode -> relevance gate.
        assert!(matches!(decide(&room(ReplyMode::Proactive, true), &msg(true, false)), Decision::Proactive));
    }
    #[test]
    fn proactive_mode_replies_when_addressed() {
        // A direct mention in proactive mode is a guaranteed reply, not gated.
        assert!(matches!(decide(&room(ReplyMode::Proactive, true), &msg(true, true)), Decision::Reply));
    }

    #[test]
    fn system_prompt_includes_house_rules_and_speaker_note() {
        let p = Personality {
            name: "sage".into(),
            label: "Sage".into(),
            description: None,
            system_prompt: "You are Sage.".into(),
            model: "qwen3:8b".into(),
            temperature: Some(0.6),
            top_p: Some(0.9),
            num_ctx: 8192,
            proactive: ProactiveConfig { relevance_threshold: 0.5, cooldown_secs: 60, max_per_hour: 4 },
            num_predict: None,
            num_ctx_override: None,
            temperature_override: None,
            top_p_override: None,
            repeat_penalty: None,
            extra_search_domains: vec![],
        };
        let r = room(ReplyMode::Addressed, true);
        let s = system_prompt(&p, &r);
        assert!(s.contains("stay in character") && s.contains("Hard limits")); // raw-tone house rules
        assert!(s.contains("square brackets")); // speaker note
        assert!(s.contains("You are Sage.")); // character preserved
    }

    fn sample_personality() -> Personality {
        Personality {
            name: "sage".into(),
            label: "Sage".into(),
            description: None,
            system_prompt: "You are Sage.".into(),
            model: "qwen3:8b".into(),
            temperature: Some(0.6),
            top_p: Some(0.9),
            num_ctx: 8192,
            proactive: ProactiveConfig { relevance_threshold: 0.5, cooldown_secs: 60, max_per_hour: 4 },
            num_predict: None,
            num_ctx_override: None,
            temperature_override: None,
            top_p_override: None,
            repeat_penalty: None,
            extra_search_domains: vec![],
        }
    }

    #[test]
    fn compose_system_includes_summary_note_when_present() {
        let p = sample_personality();
        let r = room(ReplyMode::Addressed, true);
        let s = compose_system(&p, &r, Some("Alice and Bob discussed pizza toppings."), None);
        assert!(s.contains("Earlier in this room:"));
        assert!(s.contains("pizza toppings"));
    }

    #[test]
    fn compose_system_includes_steer_directive_when_set() {
        let p = sample_personality();
        let r = room(ReplyMode::Addressed, true);
        let s = compose_system(&p, &r, None, Some("one sentence max, be nastier"));
        assert!(s.contains("OPERATOR DIRECTIVE"));
        assert!(s.contains("one sentence max, be nastier"));
        // blank steer adds no block
        assert!(!compose_system(&p, &r, None, Some("  ")).contains("OPERATOR DIRECTIVE"));
        assert!(!compose_system(&p, &r, None, None).contains("OPERATOR DIRECTIVE"));
    }

    #[test]
    fn compose_system_omits_summary_note_when_none_or_empty() {
        let p = sample_personality();
        let r = room(ReplyMode::Addressed, true);
        assert!(!compose_system(&p, &r, None, None).contains("Earlier in this room:"));
        assert!(!compose_system(&p, &r, Some("   "), None).contains("Earlier in this room:"));
    }

    #[test]
    fn current_time_note_has_prefix_and_year() {
        use super::current_time_note;
        let note = current_time_note();
        assert!(note.starts_with("Current date and time: "), "got: {note}");
        let year = chrono::Local::now().format("%Y").to_string();
        assert!(note.contains(&year), "note should contain the current year {year}: {note}");
    }

    #[test]
    fn parse_operator_command_handles_steer_and_reset() {
        use super::{parse_operator_command, OperatorCommand, SteerCommand};
        assert_eq!(parse_operator_command("!steer be nice"), Some(OperatorCommand::Steer(SteerCommand::Set("be nice".into()))));
        assert_eq!(parse_operator_command("!reset"), Some(OperatorCommand::Reset));
        assert_eq!(parse_operator_command("  !RESET  "), Some(OperatorCommand::Reset));
        assert_eq!(parse_operator_command("!reset the room now"), None); // exact !reset only
        assert_eq!(parse_operator_command("hello"), None);
    }

    #[test]
    fn parse_steer_command_variants() {
        use super::{parse_steer_command, SteerCommand};
        assert_eq!(parse_steer_command("!steer be nastier, one line"), Some(SteerCommand::Set("be nastier, one line".into())));
        assert_eq!(parse_steer_command("  !steer keep it short  "), Some(SteerCommand::Set("keep it short".into())));
        assert_eq!(parse_steer_command("!STEER One Line"), Some(SteerCommand::Set("One Line".into()))); // case-insensitive prefix
        assert_eq!(parse_steer_command("!steer"), Some(SteerCommand::Clear));
        assert_eq!(parse_steer_command("!steer clear"), Some(SteerCommand::Clear));
        assert_eq!(parse_steer_command("!steer?"), Some(SteerCommand::Show));
        assert_eq!(parse_steer_command("!steer ?"), Some(SteerCommand::Show));
        // not commands
        assert_eq!(parse_steer_command("hey what's up"), None);
        assert_eq!(parse_steer_command("!steerfoo"), None); // needs a boundary after the word
        assert_eq!(parse_steer_command("tell me about !steer"), None);
    }

    #[test]
    fn strip_own_label_removes_leading_persona_name() {
        use super::strip_own_label;
        let l = "Liberal Activist";
        assert_eq!(strip_own_label("Liberal Activist: hi there", l), "hi there");
        assert_eq!(strip_own_label("[Liberal Activist] hi there", l), "hi there");
        assert_eq!(strip_own_label("[Liberal Activist]: hi there", l), "hi there");
        assert_eq!(strip_own_label("liberal activist: yo", l), "yo"); // case-insensitive
        // unrelated text and near-misses are left alone
        assert_eq!(strip_own_label("hi there", l), "hi there");
        assert_eq!(strip_own_label("Liberal Activists are everywhere", l), "Liberal Activists are everywhere");
    }

    #[test]
    fn preview_flattens_and_truncates() {
        use super::preview;
        assert_eq!(preview("a\n  b\tc", 100), "a b c");
        let long = "x".repeat(300);
        let p = preview(&long, 10);
        assert_eq!(p.chars().count(), 11); // 10 chars + ellipsis
        assert!(p.ends_with('…'));
        assert_eq!(preview("short", 10), "short");
    }

    #[test]
    fn rate_limiter_enforces_cooldown_and_cap() {
        let rl = RateLimiter::default();
        // cooldown 60s, cap 2/hour
        assert!(rl.allow("r", 60, 2, 1000));
        assert!(!rl.allow("r", 60, 2, 1030));  // within cooldown
        assert!(rl.allow("r", 60, 2, 1070));   // cooldown passed, 2nd allowed
        assert!(!rl.allow("r", 60, 2, 1140));  // cap reached this hour
    }

    #[test]
    fn would_allow_matches_allow_without_recording() {
        let rl = RateLimiter::default();
        assert!(rl.would_allow("r", 60, 2, 1000)); // unseen room
        assert!(rl.would_allow("r", 60, 2, 1000)); // still unrecorded
        assert!(rl.allow("r", 60, 2, 1000));
        assert!(!rl.would_allow("r", 60, 2, 1030)); // within cooldown
        assert!(rl.would_allow("r", 60, 2, 1070));  // cooldown passed
        assert!(rl.allow("r", 60, 2, 1070));
        assert!(!rl.would_allow("r", 60, 2, 1140)); // hourly cap reached
    }
}
