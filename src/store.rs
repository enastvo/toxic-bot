use crate::types::{ReplyMode, Room, StoredMessage, Role};
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};

#[derive(Clone)]
pub struct Store { pool: SqlitePool }

fn now_ms() -> i64 { (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64 }

impl Store {
    pub async fn connect(url: &str) -> anyhow::Result<Store> {
        // for file DBs the setup script passes ?mode=rwc; memory works as-is
        let pool = SqlitePoolOptions::new().max_connections(5).connect(url).await?;
        sqlx::query("PRAGMA journal_mode=WAL;").execute(&pool).await.ok();
        sqlx::migrate!("./migrations").run(&pool).await?;
        Ok(Store { pool })
    }

    fn row_to_room(row: &sqlx::sqlite::SqliteRow) -> Room {
        Room {
            room_id: row.get("room_id"),
            display_name: row.get("display_name"),
            is_group: row.get::<i64, _>("is_group") != 0,
            personality: row.get("personality"),
            reply_mode: ReplyMode::parse(row.get::<String, _>("reply_mode").as_str())
                .unwrap_or(ReplyMode::Addressed),
        }
    }

    pub async fn ensure_room(&self, room_id: &str, name: Option<&str>, is_group: bool) -> anyhow::Result<Room> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO rooms (room_id, display_name, is_group, reply_mode, created_at, updated_at)
             VALUES (?, ?, ?, 'addressed', ?, ?)
             ON CONFLICT(room_id) DO UPDATE SET
               display_name = COALESCE(excluded.display_name, rooms.display_name),
               updated_at = excluded.updated_at")
            .bind(room_id).bind(name).bind(is_group as i64).bind(now).bind(now)
            .execute(&self.pool).await?;
        Ok(self.get_room(room_id).await?.expect("just inserted"))
    }

    pub async fn get_room(&self, room_id: &str) -> anyhow::Result<Option<Room>> {
        let row = sqlx::query("SELECT * FROM rooms WHERE room_id = ?")
            .bind(room_id).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| Self::row_to_room(&r)))
    }

    pub async fn list_rooms(&self) -> anyhow::Result<Vec<Room>> {
        let rows = sqlx::query("SELECT * FROM rooms ORDER BY updated_at DESC")
            .fetch_all(&self.pool).await?;
        Ok(rows.iter().map(Self::row_to_room).collect())
    }

    pub async fn set_personality(&self, room_id: &str, name: Option<&str>) -> anyhow::Result<()> {
        sqlx::query("UPDATE rooms SET personality = ?, updated_at = ? WHERE room_id = ?")
            .bind(name).bind(now_ms()).bind(room_id).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn set_reply_mode(&self, room_id: &str, mode: ReplyMode) -> anyhow::Result<()> {
        sqlx::query("UPDATE rooms SET reply_mode = ?, updated_at = ? WHERE room_id = ?")
            .bind(mode.as_str()).bind(now_ms()).bind(room_id).execute(&self.pool).await?;
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool { &self.pool }
}

pub struct NewMessage {
    pub room_id: String,
    pub sender_id: String,
    pub sender_name: Option<String>,
    pub role: Role,
    pub body: String,
    pub ts: i64,
    pub personality: Option<String>,
    pub is_mention: bool,
}

impl Store {
    pub async fn record_message(&self, m: NewMessage) -> anyhow::Result<i64> {
        let id = sqlx::query(
            "INSERT INTO messages (room_id, sender_id, sender_name, role, body, ts, personality, is_mention)
             VALUES (?,?,?,?,?,?,?,?)")
            .bind(&m.room_id).bind(&m.sender_id).bind(&m.sender_name).bind(m.role.as_str())
            .bind(&m.body).bind(m.ts).bind(&m.personality).bind(m.is_mention as i64)
            .execute(&self.pool).await?.last_insert_rowid();
        Ok(id)
    }

    fn row_to_msg(r: &sqlx::sqlite::SqliteRow) -> StoredMessage {
        use sqlx::Row;
        StoredMessage {
            id: r.get("id"), room_id: r.get("room_id"), sender_id: r.get("sender_id"),
            sender_name: r.get("sender_name"), role: Role::parse(r.get::<String,_>("role").as_str()),
            body: r.get("body"), ts: r.get("ts"), personality: r.get("personality"),
            is_mention: r.get::<i64,_>("is_mention") != 0,
        }
    }

    pub async fn recent(&self, room_id: &str, limit: i64) -> anyhow::Result<Vec<StoredMessage>> {
        // newest `limit`, then reverse to chronological
        let rows = sqlx::query("SELECT * FROM messages WHERE room_id=? ORDER BY id DESC LIMIT ?")
            .bind(room_id).bind(limit).fetch_all(&self.pool).await?;
        let mut v: Vec<_> = rows.iter().map(Self::row_to_msg).collect();
        v.reverse();
        Ok(v)
    }

    pub async fn history(&self, room_id: &str, limit: i64) -> anyhow::Result<Vec<StoredMessage>> {
        self.recent(room_id, limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ReplyMode, Role};

    async fn mem() -> Store { Store::connect("sqlite::memory:").await.unwrap() }

    #[tokio::test]
    async fn ensure_room_is_idempotent_and_defaults() {
        let s = mem().await;
        let r = s.ensure_room("g1", Some("Group One"), true).await.unwrap();
        assert_eq!(r.reply_mode, ReplyMode::Addressed);
        assert!(r.personality.is_none());
        // second call keeps existing row, updates name
        let r2 = s.ensure_room("g1", Some("Renamed"), true).await.unwrap();
        assert_eq!(r2.display_name.as_deref(), Some("Renamed"));
        assert_eq!(s.list_rooms().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn set_personality_and_mode() {
        let s = mem().await;
        s.ensure_room("g1", None, true).await.unwrap();
        s.set_personality("g1", Some("sage")).await.unwrap();
        s.set_reply_mode("g1", ReplyMode::Proactive).await.unwrap();
        let r = s.get_room("g1").await.unwrap().unwrap();
        assert_eq!(r.personality.as_deref(), Some("sage"));
        assert_eq!(r.reply_mode, ReplyMode::Proactive);
    }

    #[tokio::test]
    async fn record_and_recent_are_chronological() {
        let s = mem().await;
        s.ensure_room("g1", None, true).await.unwrap();
        for i in 0..5 {
            s.record_message(NewMessage {
                room_id: "g1".into(), sender_id: "u".into(), sender_name: Some("U".into()),
                role: Role::User, body: format!("m{i}"), ts: i, personality: None, is_mention: false,
            }).await.unwrap();
        }
        let recent = s.recent("g1", 3).await.unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent.iter().map(|m| m.body.clone()).collect::<Vec<_>>(), vec!["m2","m3","m4"]);
    }
}
