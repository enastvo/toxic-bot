use crate::types::{ReplyMode, Room, StoredMessage, Role};
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{rand_core::OsRng, SaltString};

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

    pub async fn set_admin(&self, username: &str, password: &str) -> anyhow::Result<()> {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default().hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("hash: {e}"))?.to_string();
        sqlx::query("INSERT INTO admin (id, username, password_hash) VALUES (1, ?, ?)
                     ON CONFLICT(id) DO UPDATE SET username=excluded.username, password_hash=excluded.password_hash")
            .bind(username).bind(hash).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn verify_admin(&self, username: &str, password: &str) -> anyhow::Result<bool> {
        let row = sqlx::query("SELECT username, password_hash FROM admin WHERE id = 1")
            .fetch_optional(&self.pool).await?;
        let Some(row) = row else { return Ok(false) };
        use sqlx::Row;
        if row.get::<String,_>("username") != username { return Ok(false); }
        let stored: String = row.get("password_hash");
        let parsed = PasswordHash::new(&stored).map_err(|e| anyhow::anyhow!("parse hash: {e}"))?;
        Ok(Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
    }

    pub async fn get_settings(&self) -> anyhow::Result<SettingsRow> {
        let row = sqlx::query("SELECT * FROM settings WHERE id = 1")
            .fetch_one(&self.pool).await?;
        Ok(SettingsRow {
            keep_alive: row.get("keep_alive"),
            ollama_timeout_secs: row.get("ollama_timeout_secs"),
            repeat_penalty: row.get("repeat_penalty"),
            repeat_last_n: row.get("repeat_last_n"),
            num_predict: row.get("num_predict"),
            num_ctx: row.get("num_ctx"),
            default_temperature: row.get("default_temperature"),
            default_top_p: row.get("default_top_p"),
            summary_enabled: row.get::<i64, _>("summary_enabled") != 0,
            summary_interval_hours: row.get("summary_interval_hours"),
        })
    }

    pub async fn upsert_settings(&self, s: &SettingsRow) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO settings (id, keep_alive, ollama_timeout_secs, repeat_penalty, repeat_last_n,
                                   num_predict, num_ctx, default_temperature, default_top_p, summary_enabled,
                                   summary_interval_hours)
             VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               keep_alive = excluded.keep_alive,
               ollama_timeout_secs = excluded.ollama_timeout_secs,
               repeat_penalty = excluded.repeat_penalty,
               repeat_last_n = excluded.repeat_last_n,
               num_predict = excluded.num_predict,
               num_ctx = excluded.num_ctx,
               default_temperature = excluded.default_temperature,
               default_top_p = excluded.default_top_p,
               summary_enabled = excluded.summary_enabled,
               summary_interval_hours = excluded.summary_interval_hours")
            .bind(&s.keep_alive)
            .bind(s.ollama_timeout_secs)
            .bind(s.repeat_penalty)
            .bind(s.repeat_last_n)
            .bind(s.num_predict)
            .bind(s.num_ctx)
            .bind(s.default_temperature)
            .bind(s.default_top_p)
            .bind(s.summary_enabled as i64)
            .bind(s.summary_interval_hours)
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn settings_exists(&self) -> anyhow::Result<bool> {
        let row = sqlx::query("SELECT COUNT(*) as cnt FROM settings WHERE id = 1")
            .fetch_one(&self.pool).await?;
        Ok(row.get::<i64, _>("cnt") > 0)
    }

    pub fn pool(&self) -> &SqlitePool { &self.pool }
}

#[derive(Clone, Debug)]
pub struct RoomSummary {
    pub summary: String,
    pub covered_through_ts: i64,
}

#[derive(Clone, Debug)]
pub struct SettingsRow {
    pub keep_alive: String,
    pub ollama_timeout_secs: i64,
    pub repeat_penalty: f64,
    pub repeat_last_n: i64,
    pub num_predict: i64,
    pub num_ctx: i64,
    pub default_temperature: f64,
    pub default_top_p: f64,
    pub summary_enabled: bool,
    pub summary_interval_hours: i64,
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

    pub async fn get_summary(&self, room_id: &str) -> anyhow::Result<Option<RoomSummary>> {
        let row = sqlx::query("SELECT * FROM room_summaries WHERE room_id = ?")
            .bind(room_id).fetch_optional(&self.pool).await?;
        Ok(row.map(|r| RoomSummary {
            summary: r.get("summary"),
            covered_through_ts: r.get("covered_through_ts"),
        }))
    }

    pub async fn upsert_summary(&self, room_id: &str, summary: &str, covered_through_ts: i64) -> anyhow::Result<()> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO room_summaries (room_id, summary, covered_through_ts, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(room_id) DO UPDATE SET
               summary = excluded.summary,
               covered_through_ts = excluded.covered_through_ts,
               updated_at = excluded.updated_at")
            .bind(room_id).bind(summary).bind(covered_through_ts).bind(now)
            .execute(&self.pool).await?;
        Ok(())
    }

    pub async fn rooms_with_new_messages_since_summary(&self) -> anyhow::Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT m.room_id FROM messages m
             LEFT JOIN room_summaries s ON m.room_id = s.room_id
             GROUP BY m.room_id
             HAVING MAX(m.ts) > COALESCE(s.covered_through_ts, -1)")
            .fetch_all(&self.pool).await?;
        Ok(rows.iter().map(|r| r.get("room_id")).collect())
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

    #[tokio::test]
    async fn admin_password_roundtrip() {
        let s = mem().await;
        s.set_admin("admin", "s3cret").await.unwrap();
        assert!(s.verify_admin("admin", "s3cret").await.unwrap());
        assert!(!s.verify_admin("admin", "wrong").await.unwrap());
        assert!(!s.verify_admin("nobody", "s3cret").await.unwrap());
    }

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

    #[tokio::test]
    async fn summary_roundtrip() {
        let s = mem().await;
        s.ensure_room("g1", None, true).await.unwrap();
        // Before upsert, get returns None
        assert!(s.get_summary("g1").await.unwrap().is_none());
        // After upsert, get returns the values
        s.upsert_summary("g1", "test summary", 100).await.unwrap();
        let got = s.get_summary("g1").await.unwrap().unwrap();
        assert_eq!(got.summary, "test summary");
        assert_eq!(got.covered_through_ts, 100);
        // Second upsert updates the values
        s.upsert_summary("g1", "updated summary", 200).await.unwrap();
        let got2 = s.get_summary("g1").await.unwrap().unwrap();
        assert_eq!(got2.summary, "updated summary");
        assert_eq!(got2.covered_through_ts, 200);
    }

    #[tokio::test]
    async fn rooms_with_new_messages_since_summary() {
        let s = mem().await;
        s.ensure_room("g1", None, true).await.unwrap();
        s.ensure_room("g2", None, true).await.unwrap();

        // No messages yet, should return empty
        assert!(s.rooms_with_new_messages_since_summary().await.unwrap().is_empty());

        // Record messages in g1
        s.record_message(NewMessage {
            room_id: "g1".into(), sender_id: "u".into(), sender_name: Some("U".into()),
            role: Role::User, body: "m1".into(), ts: 100, personality: None, is_mention: false,
        }).await.unwrap();

        // Now g1 should be returned (has messages but no summary)
        let rooms = s.rooms_with_new_messages_since_summary().await.unwrap();
        assert!(rooms.contains(&"g1".to_string()));
        assert!(!rooms.contains(&"g2".to_string()));

        // Record more messages in g1
        s.record_message(NewMessage {
            room_id: "g1".into(), sender_id: "u".into(), sender_name: Some("U".into()),
            role: Role::User, body: "m2".into(), ts: 150, personality: None, is_mention: false,
        }).await.unwrap();

        // Still should return g1 since max ts (150) > covered_through_ts (0 default)
        let rooms = s.rooms_with_new_messages_since_summary().await.unwrap();
        assert!(rooms.contains(&"g1".to_string()));

        // Upsert summary with covered_through_ts >= max ts
        s.upsert_summary("g1", "summary", 150).await.unwrap();

        // Now g1 should NOT be returned since covered_through_ts >= max ts
        let rooms = s.rooms_with_new_messages_since_summary().await.unwrap();
        assert!(!rooms.contains(&"g1".to_string()));

        // Record new message with higher ts
        s.record_message(NewMessage {
            room_id: "g1".into(), sender_id: "u".into(), sender_name: Some("U".into()),
            role: Role::User, body: "m3".into(), ts: 200, personality: None, is_mention: false,
        }).await.unwrap();

        // Now g1 should be returned again since max ts (200) > covered_through_ts (150)
        let rooms = s.rooms_with_new_messages_since_summary().await.unwrap();
        assert!(rooms.contains(&"g1".to_string()));
    }
}
