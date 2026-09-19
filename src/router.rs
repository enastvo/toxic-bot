use crate::llm::{ChatRequest, LlmBackend};
use crate::personalities::{Personalities, Personality};
use crate::signal::SignalTransport;
use crate::store::{NewMessage, Store};
use crate::types::{ChatTurn, IncomingMessage, ReplyMode, Role, Room, StoredMessage};
use crate::window::select_window;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision { Reply, Proactive, Silent }

pub fn decide(room: &Room, msg: &IncomingMessage) -> Decision {
    if !room.is_group { return Decision::Reply; }
    match room.reply_mode {
        ReplyMode::Always => Decision::Reply,
        ReplyMode::Addressed => if msg.is_mention || msg.quoted_msg.is_some() { Decision::Reply } else { Decision::Silent },
        ReplyMode::Proactive => Decision::Proactive,
    }
}

#[derive(Default)]
pub struct RateLimiter { rooms: Mutex<HashMap<String, RoomRate>> }
#[derive(Default)]
struct RoomRate { last: u64, hits: Vec<u64> }

impl RateLimiter {
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

pub fn build_turns(window: &[StoredMessage], bot_id: &str) -> Vec<ChatTurn> {
    window.iter().map(|m| {
        if m.role == Role::Assistant || m.sender_id == bot_id {
            ChatTurn { role: Role::Assistant, name: None, content: m.body.clone() }
        } else {
            ChatTurn { role: Role::User, name: m.sender_name.clone().or_else(|| Some(m.sender_id.clone())), content: m.body.clone() }
        }
    }).collect()
}

pub fn system_prompt(personality: &Personality, room: &Room) -> String {
    if room.is_group {
        let name = room.display_name.as_deref().unwrap_or("a group");
        format!("{}\n\nYou are in a Signal group named \"{}\". Multiple people talk here; each user message is prefixed with the speaker's name.", personality.system_prompt.trim(), name)
    } else {
        personality.system_prompt.trim().to_string()
    }
}

pub struct Router {
    store: Store,
    personalities: Arc<Personalities>,
    llm: Arc<dyn LlmBackend>,
    signal: Arc<dyn SignalTransport>,
    bot_id: String,
    dry_run: bool,
    rl: RateLimiter,
}

fn now_secs() -> u64 { time::OffsetDateTime::now_utc().unix_timestamp() as u64 }

impl Router {
    pub fn new(store: Store, personalities: Arc<Personalities>, llm: Arc<dyn LlmBackend>,
               signal: Arc<dyn SignalTransport>, bot_id: String, dry_run: bool) -> Self {
        Self { store, personalities, llm, signal, bot_id, dry_run, rl: RateLimiter::default() }
    }

    pub async fn handle(&self, msg: IncomingMessage) -> anyhow::Result<Option<String>> {
        let room = self.store.ensure_room(&msg.room_id, msg.sender_name.as_deref().filter(|_| !msg.is_group), msg.is_group).await?;
        self.store.record_message(NewMessage {
            room_id: msg.room_id.clone(), sender_id: msg.sender_id.clone(), sender_name: msg.sender_name.clone(),
            role: Role::User, body: msg.body.clone(), ts: msg.timestamp, personality: None, is_mention: msg.is_mention,
        }).await?;

        let personality = self.personalities.get_or_default(room.personality.as_deref());

        let decision = decide(&room, &msg);
        tracing::debug!(room=%room.room_id, ?decision, mode=?room.reply_mode, "routing");
        if decision == Decision::Silent { return Ok(None); }

        // build context window
        let recent = self.store.recent(&room.room_id, 60).await?;
        let window = select_window(&recent, personality.num_ctx);
        let turns = build_turns(&window, &self.bot_id);

        if decision == Decision::Proactive {
            let rel = self.llm.relevance_check(&personality.model, personality.num_ctx, turns.clone()).await?;
            tracing::debug!(room=%room.room_id, should=rel.should_reply, conf=rel.confidence, thr=personality.proactive.relevance_threshold, "relevance");
            if !rel.should_reply || rel.confidence < personality.proactive.relevance_threshold { return Ok(None); }
            if !self.dry_run && !self.rl.allow(&room.room_id, personality.proactive.cooldown_secs, personality.proactive.max_per_hour, now_secs()) {
                tracing::debug!(room=%room.room_id, "proactive rate-limited");
                return Ok(None);
            }
        }

        let reply = self.llm.generate_reply(ChatRequest {
            model: personality.model.clone(),
            system: system_prompt(&personality, &room),
            turns,
            temperature: personality.temperature,
            top_p: personality.top_p,
            num_ctx: personality.num_ctx,
        }).await?;

        if reply.trim().is_empty() { return Ok(None); }

        if self.dry_run {
            tracing::info!(room=%room.room_id, %reply, "[dry-run] would send");
            return Ok(Some(reply));
        }

        self.store.record_message(NewMessage {
            room_id: room.room_id.clone(), sender_id: self.bot_id.clone(), sender_name: Some(personality.label.clone()),
            role: Role::Assistant, body: reply.clone(), ts: now_secs() as i64 * 1000,
            personality: Some(personality.name.clone()), is_mention: false,
        }).await?;
        self.signal.send(&room.room_id, room.is_group, &reply).await?;
        Ok(Some(reply))
    }
}

#[cfg(test)]
mod tests {
    use super::{Decision, RateLimiter, decide, build_turns};
    use crate::types::{Room, ReplyMode, IncomingMessage, StoredMessage, Role};

    fn room(mode: ReplyMode, is_group: bool) -> Room {
        Room { room_id: "r".into(), display_name: None, is_group, personality: None, reply_mode: mode }
    }
    fn msg(is_group: bool, mention: bool) -> IncomingMessage {
        IncomingMessage { room_id: "r".into(), sender_id: "u".into(), sender_name: None, body: "hi".into(),
            is_group, is_mention: mention, quoted_msg: None, timestamp: 0 }
    }

    fn sm(id: i64, sender: &str, role: Role, body: &str) -> StoredMessage {
        StoredMessage { id, room_id: "r".into(), sender_id: sender.into(), sender_name: Some(sender.into()),
            role, body: body.into(), ts: id, personality: None, is_mention: false }
    }

    #[test]
    fn build_turns_marks_bot_as_assistant() {
        let w = vec![ sm(1, "+1000", Role::User, "hi"), sm(2, "+bot", Role::Assistant, "hello") ];
        let turns = build_turns(&w, "+bot");
        assert_eq!(turns[0].role, Role::User);
        assert_eq!(turns[0].name.as_deref(), Some("+1000"));
        assert_eq!(turns[1].role, Role::Assistant);
        assert!(turns[1].name.is_none());
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
        assert!(matches!(decide(&room(ReplyMode::Proactive, true), &msg(true, false)), Decision::Proactive));
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
}
