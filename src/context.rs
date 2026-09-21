//! Layered context assembly: caps the bot's own self-history within the
//! recent-message window and labels speakers so a group chat with multiple
//! humans doesn't get its turns conflated by the model.

use crate::types::{ChatTurn, Role, StoredMessage};

/// Max number of messages (human + bot) kept in the final context window.
pub const MAX_WINDOW_MSGS: usize = 18;
/// Max number of the bot's own prior turns kept in that window.
pub const MAX_BOT_TURNS: usize = 2;

fn is_bot_msg(m: &StoredMessage, bot_id: &str) -> bool {
    m.sender_id == bot_id || m.role == Role::Assistant
}

/// Build the list of chat turns to send to the LLM from the chronological
/// recent-message history. Drops all-but-the-last `MAX_BOT_TURNS` of the
/// bot's own messages (keeping all human messages), then keeps at most the
/// last `MAX_WINDOW_MSGS` of what remains, preserving chronological order
/// throughout.
pub fn build_context_turns(recent_chrono: &[StoredMessage], bot_id: &str) -> Vec<ChatTurn> {
    let bot_count = recent_chrono.iter().filter(|m| is_bot_msg(m, bot_id)).count();
    let mut drop_bot = bot_count.saturating_sub(MAX_BOT_TURNS);

    // First pass: drop all-but-the-last MAX_BOT_TURNS bot messages, keeping
    // all human messages, in original chronological order.
    let mut capped: Vec<&StoredMessage> = Vec::with_capacity(recent_chrono.len());
    for m in recent_chrono {
        if is_bot_msg(m, bot_id) && drop_bot > 0 {
            drop_bot -= 1;
            continue;
        }
        capped.push(m);
    }

    // Second pass: keep at most the last MAX_WINDOW_MSGS, preserving order.
    let start = capped.len().saturating_sub(MAX_WINDOW_MSGS);
    capped[start..].iter().map(|m| to_turn(m, bot_id)).collect()
}

fn to_turn(m: &StoredMessage, bot_id: &str) -> ChatTurn {
    if is_bot_msg(m, bot_id) {
        let label = match &m.sender_name {
            Some(name) => format!("[you, as {}]", name),
            None => "[you]".to_string(),
        };
        ChatTurn { role: Role::Assistant, name: Some(label), content: m.body.clone() }
    } else {
        let name = m.sender_name.clone().unwrap_or_else(|| m.sender_id.clone());
        ChatTurn { role: Role::User, name: Some(format!("[{}]", name)), content: m.body.clone() }
    }
}

/// A system-prompt line explaining the `[Name]:` speaker labels used in the
/// chat history, so the model doesn't conflate different speakers.
pub fn speaker_note() -> &'static str {
    "Messages in this conversation are labeled with the speaker's name in square brackets, \
like \"[Alice]: text\". Multiple different people may be talking here — never attribute one \
person's words or intentions to another; keep each speaker's statements attached to their own label. \
These labels are for your understanding only: do NOT copy them into your reply. Write only your \
message, with no name label or bracketed prefix of your own — address people by their name \
naturally within your sentences."
}

/// Approximate cap (in characters) for the long-term summary note, standing
/// in for a ~250-token budget (roughly 4 chars/token).
const SUMMARY_CHAR_CAP: usize = 1000;

/// Build a token-capped system note carrying the room's rolling long-term
/// summary, for injection ahead of the personality's system prompt. Returns
/// `None` when the summary is empty (nothing to inject).
pub fn summary_block(summary: &str) -> Option<String> {
    let trimmed = summary.trim();
    if trimmed.is_empty() {
        return None;
    }
    let body = if trimmed.chars().count() > SUMMARY_CHAR_CAP {
        let truncated: String = trimmed.chars().take(SUMMARY_CHAR_CAP).collect();
        format!("{truncated}…")
    } else {
        trimmed.to_string()
    };
    Some(format!("Earlier in this room: {body}"))
}

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

    #[test]
    fn summary_block_empty_is_none() {
        assert!(summary_block("").is_none());
        assert!(summary_block("   ").is_none());
    }

    #[test]
    fn summary_block_truncates_long_summary() {
        let long = "word ".repeat(1000); // way over the ~250-token / 1000-char cap
        let s = summary_block(&long).unwrap();
        assert!(s.starts_with("Earlier in this room:"));
        // capped: total length should be bounded well under the raw input's length.
        assert!(s.len() < long.len());
        assert!(s.ends_with('…'));
    }

    #[test]
    fn summary_block_normal_summary() {
        let s = summary_block("Alice and Bob discussed pizza toppings.").unwrap();
        assert!(s.contains("Earlier in this room:"));
        assert!(s.contains("pizza toppings"));
    }
}
