use crate::types::StoredMessage;

const FLOOR: usize = 8;
const CEILING: usize = 60;

fn approx_tokens(s: &str) -> usize { (s.len() / 4).max(1) }

/// `msgs` chronological (oldest first). Returns newest slice within budget.
pub fn select_window(msgs: &[StoredMessage], num_ctx: u32) -> Vec<StoredMessage> {
    let budget = ((num_ctx as f64) * 0.75) as usize;
    let mut used = 0usize;
    let mut take = 0usize;
    for m in msgs.iter().rev() {
        let t = approx_tokens(&m.body);
        if take >= FLOOR && (used + t > budget || take >= CEILING) { break; }
        used += t; take += 1;
        if take >= CEILING { break; }
    }
    let take = take.min(msgs.len());
    msgs[msgs.len() - take..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;

    fn msg(id: i64, body: &str) -> StoredMessage {
        StoredMessage { id, room_id: "r".into(), sender_id: "s".into(), sender_name: None,
            role: Role::User, body: body.into(), ts: id, personality: None, is_mention: false }
    }

    #[test]
    fn keeps_last_n_within_budget() {
        // 100 messages of ~40 chars = ~10 tokens each. num_ctx 1000 -> budget 750 tokens -> ~75 msgs, capped at 60.
        let all: Vec<_> = (0..100).map(|i| msg(i, &"x".repeat(40))).collect();
        let w = select_window(&all, 1000);
        assert_eq!(w.len(), 60); // ceiling
        assert_eq!(w.first().unwrap().id, 40); // newest 60, chronological
        assert_eq!(w.last().unwrap().id, 99);
    }

    #[test]
    fn floor_is_eight_even_when_budget_tiny() {
        let all: Vec<_> = (0..20).map(|i| msg(i, &"y".repeat(400))).collect();
        let w = select_window(&all, 100); // budget ~75 tokens, one msg ~100 tokens
        assert_eq!(w.len(), 8); // floor
        assert_eq!(w.last().unwrap().id, 19);
    }

    #[test]
    fn returns_all_when_few() {
        let all: Vec<_> = (0..3).map(|i| msg(i, "hi")).collect();
        let w = select_window(&all, 8192);
        assert_eq!(w.len(), 3);
    }
}
