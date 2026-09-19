use crate::types::{IncomingMessage, ReplyMode, Room};
use std::collections::HashMap;
use std::sync::Mutex;

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

#[cfg(test)]
mod tests {
    use super::{Decision, RateLimiter, decide};
    use crate::types::{Room, ReplyMode, IncomingMessage};

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
