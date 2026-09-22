# Signal Chat Bot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a self-hosted Rust Signal bot that replies via a local `qwen3:8b` (Ollama), wears a per-room personality, supports addressed/always/proactive reply modes, and exposes a LAN-only TLS dashboard to view history and assign personalities.

**Architecture:** One `tokio` binary. A `signal` task supervises `signal-cli --jsonrpc` over a UNIX socket and emits normalized messages; a `router` applies per-room policy, builds prompts from a rolling window, and calls Ollama; a `store` (SQLite/WAL) persists rooms and messages; a `web` task serves an axum+rustls dashboard. `SignalTransport` and `LlmBackend` are traits with real + mock impls so the whole pipeline is testable without Signal or Ollama.

**Tech Stack:** Rust, tokio, sqlx (SQLite), serde/toml, reqwest (rustls), axum + axum-server (rustls), askama, argon2, tower-sessions, notify, clap, tracing, rcgen, async-trait, thiserror/anyhow.

**Spec:** `docs/superpowers/specs/2026-09-19-signal-bot-design.md`

## Global Constraints

- Language: Rust (edition 2021), single binary crate `signal-bot` with a `lib.rs` exposing modules + a thin `main.rs`.
- TLS everywhere the network is exposed; the **only** inbound listener is the axum HTTPS port bound to the LAN interface. signal-cli socket and Ollama (`127.0.0.1:11434`) are local-only.
- Bot Signal number: `+15555550100` (Google Voice), one identity.
- LLM model default: `qwen3:8b` via Ollama HTTP chat API.
- Reply modes (per room): `addressed` | `always` | `proactive`. Default `addressed`.
- Proactive gating: cheap `relevance_check` (JSON `{should_reply, confidence}`) + per-room `cooldown_secs` and `max_per_hour` from the personality's `[proactive]` block; only reply if `confidence >= relevance_threshold`.
- Memory: rolling window = newest messages up to ~75% of `num_ctx` (approx tokens = chars/4), hard floor 8 messages, ceiling 60.
- Personalities: `personalities/*.toml`, filename = name; a `default` is required (fatal if missing/invalid); other invalid files are skipped and logged.
- Web: LAN-only, single admin (argon2 hash), cookie session (HttpOnly, Secure, SameSite=Strict), rate-limited login. Dashboard = view history + set active personality + set reply mode. Live updates via SSE. Server-rendered `askama` templates + minimal vanilla JS.
- Runtime user: unprivileged `signal-bot` system account; data dir `/var/lib/signal-bot` mode `0700`. Provisioning via the `bot` admin (sudo).
- Never crash the whole process: each task supervised, panics restart the task. Errors logged via `tracing`.
- Secrets never committed (`.gitignore` already covers `*.pem`, `*.key`, `config.local.toml`, `/data/`, `/signal-data/`).
- Frequent commits: every task ends with a commit. Tests must pass before commit.

## File Structure

```
signal-bot/
  Cargo.toml
  src/
    main.rs            # CLI parse, config load, wire tasks, supervise
    lib.rs             # pub mod re-exports
    config.rs          # AppConfig (TOML + CLI flags), paths, flags (debug/dry_run/repl)
    types.rs           # IncomingMessage, Room, ReplyMode, Role, StoredMessage, ChatTurn
    store.rs           # Store: SQLite pool + typed methods; migrations
    personalities.rs   # Personality, ProactiveConfig, Personalities loader + hot-reload
    llm.rs             # LlmBackend trait, OllamaClient, MockLlm; ChatRequest/Relevance
    signal.rs          # SignalTransport trait, SignalCli (jsonrpc child), MockSignal
    router.rs          # Router: policy, rate limiting, prompt build, orchestration
    window.rs          # rolling-window selection (pure fn, unit-tested)
    web/
      mod.rs           # axum app builder, TLS server, routes table
      auth.rs          # session, login/logout, admin credential, rate limit
      handlers.rs      # room list, room detail, set personality/mode, SSE
    repl.rs            # --repl harness feeding MockSignal into Router
  migrations/          # sqlx migrations (0001_init.sql)
  templates/           # askama: base.html, login.html, rooms.html, room.html
  personalities/       # default.toml, sage.toml (example)
  deploy/
    signal-bot.service # systemd unit
    setup.sh           # provisioning script (run by bot admin, sudo)
    gen-cert.sh        # local CA + bot.local cert (rcgen or mkcert)
    REGISTER.md        # manual Signal registration steps
  tests/
    router_it.rs       # integration: Mock signal+llm + in-memory store
    web_it.rs          # integration: axum handlers via tower::ServiceExt
```

Files that change together live together (`web/` submodule groups the HTTP layer; `window.rs` isolates pure selection logic for cheap unit tests).

---

## Phase 0 — Scaffolding

### Task 0: Cargo crate + CI-less test harness

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/main.rs`, `src/types.rs`

**Interfaces:**
- Produces: crate compiles; `cargo test` runs (zero tests initially); `types` module with the enums/structs later tasks consume.

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "signal-bot"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1", features = ["full"] }
sqlx = { version = "0.8", default-features = false, features = ["runtime-tokio-rustls", "sqlite", "macros", "migrate"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
axum = { version = "0.7", features = ["macros"] }
axum-server = { version = "0.7", features = ["tls-rustls"] }
tower = "0.5"
tower-http = { version = "0.6", features = ["trace", "fs"] }
tower-sessions = "0.13"
askama = "0.12"
argon2 = "0.5"
notify = "6"
clap = { version = "4", features = ["derive", "env"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
async-trait = "0.1"
thiserror = "2"
anyhow = "1"
rcgen = "0.13"
futures = "0.3"
time = { version = "0.3", features = ["formatting"] }

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Create `src/types.rs`**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplyMode { Addressed, Always, Proactive }

impl ReplyMode {
    pub fn as_str(self) -> &'static str {
        match self { Self::Addressed => "addressed", Self::Always => "always", Self::Proactive => "proactive" }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s { "addressed" => Some(Self::Addressed), "always" => Some(Self::Always), "proactive" => Some(Self::Proactive), _ => None }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role { User, Assistant }

impl Role {
    pub fn as_str(self) -> &'static str { match self { Self::User => "user", Self::Assistant => "assistant" } }
    pub fn parse(s: &str) -> Role { if s == "assistant" { Role::Assistant } else { Role::User } }
}

#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub room_id: String,
    pub sender_id: String,
    pub sender_name: Option<String>,
    pub body: String,
    pub is_group: bool,
    pub is_mention: bool,
    pub quoted_msg: Option<String>,
    pub timestamp: i64, // ms since epoch (signal server ts)
}

#[derive(Debug, Clone)]
pub struct Room {
    pub room_id: String,
    pub display_name: Option<String>,
    pub is_group: bool,
    pub personality: Option<String>,
    pub reply_mode: ReplyMode,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: i64,
    pub room_id: String,
    pub sender_id: String,
    pub sender_name: Option<String>,
    pub role: Role,
    pub body: String,
    pub ts: i64,
    pub personality: Option<String>,
    pub is_mention: bool,
}

/// One turn as sent to the LLM.
#[derive(Debug, Clone)]
pub struct ChatTurn { pub role: Role, pub name: Option<String>, pub content: String }
```

- [ ] **Step 3: Create `src/lib.rs`**

```rust
pub mod types;
// modules added as tasks land:
// pub mod config; pub mod store; pub mod personalities; pub mod llm;
// pub mod signal; pub mod window; pub mod router; pub mod repl; pub mod web;
```

- [ ] **Step 4: Create `src/main.rs`**

```rust
fn main() { println!("signal-bot placeholder"); }
```

- [ ] **Step 5: Build and test**

Run: `cargo build && cargo test`
Expected: builds; `test result: ok. 0 passed`.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml src/
git commit -m "chore: scaffold signal-bot crate and core types"
```

---

## Phase 1 — Store (SQLite)

### Task 1: Rolling-window selection (pure function)

**Files:**
- Create: `src/window.rs`
- Modify: `src/lib.rs` (add `pub mod window;`)

**Interfaces:**
- Produces: `pub fn select_window(msgs: &[StoredMessage], num_ctx: u32) -> Vec<StoredMessage>` — takes chronological messages (oldest→newest), returns the newest slice within the token budget (~75% of `num_ctx`, chars/4), floor 8, ceiling 60, preserving chronological order.

- [ ] **Step 1: Write the failing test** — append to `src/window.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StoredMessage, Role};

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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test window::`
Expected: FAIL — `select_window` not found.

- [ ] **Step 3: Write minimal implementation** — top of `src/window.rs`:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test window::`
Expected: PASS (3 tests). Add `pub mod window;` to `src/lib.rs`.

- [ ] **Step 5: Commit**

```bash
git add src/window.rs src/lib.rs
git commit -m "feat: rolling-window selection with token budget, floor, ceiling"
```

### Task 2: Store — migrations + rooms

**Files:**
- Create: `src/store.rs`, `migrations/0001_init.sql`
- Modify: `src/lib.rs` (add `pub mod store;`)

**Interfaces:**
- Produces:
  - `Store::connect(url: &str) -> anyhow::Result<Store>` (runs migrations; use `"sqlite::memory:"` in tests)
  - `async fn ensure_room(&self, room_id, display_name: Option<&str>, is_group: bool) -> anyhow::Result<Room>`
  - `async fn get_room(&self, room_id: &str) -> anyhow::Result<Option<Room>>`
  - `async fn list_rooms(&self) -> anyhow::Result<Vec<Room>>`
  - `async fn set_personality(&self, room_id, name: Option<&str>) -> anyhow::Result<()>`
  - `async fn set_reply_mode(&self, room_id, mode: ReplyMode) -> anyhow::Result<()>`

- [ ] **Step 1: Create `migrations/0001_init.sql`**

```sql
CREATE TABLE rooms (
    room_id      TEXT PRIMARY KEY,
    display_name TEXT,
    is_group     INTEGER NOT NULL,
    personality  TEXT,
    reply_mode   TEXT NOT NULL DEFAULT 'addressed',
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);
CREATE TABLE messages (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id      TEXT NOT NULL REFERENCES rooms(room_id),
    sender_id    TEXT NOT NULL,
    sender_name  TEXT,
    role         TEXT NOT NULL,
    body         TEXT NOT NULL,
    ts           INTEGER NOT NULL,
    personality  TEXT,
    is_mention   INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_messages_room_ts ON messages(room_id, ts);
CREATE TABLE admin (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    username      TEXT NOT NULL,
    password_hash TEXT NOT NULL
);
```

- [ ] **Step 2: Write the failing test** — append to `src/store.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ReplyMode;

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
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test store::`
Expected: FAIL — `Store` not found.

- [ ] **Step 4: Write minimal implementation** — top of `src/store.rs`:

```rust
use crate::types::{ReplyMode, Room};
use sqlx::{sqlite::SqlitePoolOptions, Row, SqlitePool};

#[derive(Clone)]
pub struct Store { pool: SqlitePool }

fn now_ms() -> i64 { time::OffsetDateTime::now_utc().unix_timestamp() * 1000 }

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
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test store::`
Expected: PASS. Add `pub mod store;` to `src/lib.rs`.

- [ ] **Step 6: Commit**

```bash
git add src/store.rs migrations/ src/lib.rs
git commit -m "feat: store with migrations, rooms, personality/mode assignment"
```

### Task 3: Store — messages + recent window query

**Files:**
- Modify: `src/store.rs`

**Interfaces:**
- Produces:
  - `pub struct NewMessage { room_id, sender_id, sender_name: Option<String>, role: Role, body: String, ts: i64, personality: Option<String>, is_mention: bool }`
  - `async fn record_message(&self, m: NewMessage) -> anyhow::Result<i64>`
  - `async fn recent(&self, room_id: &str, limit: i64) -> anyhow::Result<Vec<StoredMessage>>` (chronological asc)
  - `async fn history(&self, room_id: &str, limit: i64) -> anyhow::Result<Vec<StoredMessage>>` (chronological asc, larger limit for dashboard)

- [ ] **Step 1: Write the failing test** — add to the `tests` mod in `src/store.rs`:

```rust
    use crate::types::Role;

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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test store::record_and_recent`
Expected: FAIL — `NewMessage` / `record_message` not found.

- [ ] **Step 3: Write minimal implementation** — add to `src/store.rs` (import `StoredMessage`, `Role`):

```rust
use crate::types::{StoredMessage, Role};

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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test store::`
Expected: PASS (all store tests).

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat: store message recording and recent-window query"
```

---

## Phase 2 — Personalities

### Task 4: Personality TOML loader

**Files:**
- Create: `src/personalities.rs`, `personalities/default.toml`, `personalities/sage.toml`
- Modify: `src/lib.rs` (add `pub mod personalities;`)

**Interfaces:**
- Produces:
  - `pub struct Personality { name, label, description: Option<String>, system_prompt, model, temperature: f32, top_p: f32, num_ctx: u32, proactive: ProactiveConfig }`
  - `pub struct ProactiveConfig { relevance_threshold: f32, cooldown_secs: u64, max_per_hour: u32 }`
  - `Personalities::load_dir(dir: &Path) -> anyhow::Result<Personalities>` (errors if `default` missing/invalid; skips+logs other bad files)
  - `fn get(&self, name: &str) -> Option<Arc<Personality>>`
  - `fn get_or_default(&self, name: Option<&str>) -> Arc<Personality>`
  - `fn list(&self) -> Vec<Arc<Personality>>`
  - `fn replace(&self, other: Personalities)` behind an internal `ArcSwap`/`RwLock` for hot-reload (Task 15)

- [ ] **Step 1: Create `personalities/default.toml`**

```toml
label = "Default"
description = "Neutral, helpful assistant."
system_prompt = """
You are a helpful assistant in a Signal chat. Keep replies concise and
friendly. If you are unsure, say so.
"""
model = "qwen3:8b"
temperature = 0.6
top_p = 0.9
num_ctx = 8192

[proactive]
relevance_threshold = 0.7
cooldown_secs = 120
max_per_hour = 6
```

- [ ] **Step 2: Create `personalities/sage.toml`**

```toml
label = "Sage"
description = "Calm, terse technical advisor."
system_prompt = """
You are Sage, a calm and precise technical advisor. Be concise. Prefer
concrete examples. If unsure, say so.
"""
model = "qwen3:8b"
temperature = 0.4
top_p = 0.9
num_ctx = 8192

[proactive]
relevance_threshold = 0.75
cooldown_secs = 180
max_per_hour = 4
```

- [ ] **Step 3: Write the failing test** — append to `src/personalities.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        let mut f = std::fs::File::create(dir.join(name)).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    const OK: &str = r#"label="X"
system_prompt="hi"
model="qwen3:8b"
temperature=0.5
top_p=0.9
num_ctx=4096
[proactive]
relevance_threshold=0.7
cooldown_secs=60
max_per_hour=5
"#;

    #[test]
    fn loads_and_requires_default() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "default.toml", OK);
        write(d.path(), "sage.toml", OK);
        let p = Personalities::load_dir(d.path()).unwrap();
        assert_eq!(p.list().len(), 2);
        assert_eq!(p.get("sage").unwrap().name, "sage");
        // unknown falls back to default
        assert_eq!(p.get_or_default(Some("nope")).name, "default");
    }

    #[test]
    fn missing_default_is_error() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "sage.toml", OK);
        assert!(Personalities::load_dir(d.path()).is_err());
    }

    #[test]
    fn bad_file_is_skipped_not_fatal() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "default.toml", OK);
        write(d.path(), "broken.toml", "not valid = = toml");
        let p = Personalities::load_dir(d.path()).unwrap();
        assert!(p.get("broken").is_none());
        assert!(p.get("default").is_some());
    }
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test personalities::`
Expected: FAIL — `Personalities` not found.

- [ ] **Step 5: Write minimal implementation** — top of `src/personalities.rs`:

```rust
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Deserialize)]
pub struct ProactiveConfig {
    pub relevance_threshold: f32,
    pub cooldown_secs: u64,
    pub max_per_hour: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Personality {
    #[serde(skip)] pub name: String,
    pub label: String,
    #[serde(default)] pub description: Option<String>,
    pub system_prompt: String,
    #[serde(default = "default_model")] pub model: String,
    #[serde(default = "def_temp")] pub temperature: f32,
    #[serde(default = "def_top_p")] pub top_p: f32,
    #[serde(default = "def_ctx")] pub num_ctx: u32,
    pub proactive: ProactiveConfig,
}
fn default_model() -> String { "qwen3:8b".into() }
fn def_temp() -> f32 { 0.6 }
fn def_top_p() -> f32 { 0.9 }
fn def_ctx() -> u32 { 8192 }

pub struct Personalities { inner: RwLock<Arc<HashMap<String, Arc<Personality>>>> }

impl Personalities {
    pub fn load_dir(dir: &Path) -> anyhow::Result<Personalities> {
        let map = Self::read_map(dir)?;
        if !map.contains_key("default") {
            anyhow::bail!("required personality 'default' not found in {}", dir.display());
        }
        Ok(Personalities { inner: RwLock::new(Arc::new(map)) })
    }

    fn read_map(dir: &Path) -> anyhow::Result<HashMap<String, Arc<Personality>>> {
        let mut map = HashMap::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") { continue; }
            let name = path.file_stem().unwrap().to_string_lossy().to_string();
            let text = std::fs::read_to_string(&path)?;
            match toml::from_str::<Personality>(&text) {
                Ok(mut p) => { p.name = name.clone(); map.insert(name, Arc::new(p)); }
                Err(e) => tracing::warn!("skipping personality {}: {e}", path.display()),
            }
        }
        Ok(map)
    }

    pub fn reload(&self, dir: &Path) -> anyhow::Result<()> {
        let map = Self::read_map(dir)?;
        if !map.contains_key("default") { anyhow::bail!("reload aborted: 'default' missing"); }
        *self.inner.write().unwrap() = Arc::new(map);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<Arc<Personality>> {
        self.inner.read().unwrap().get(name).cloned()
    }
    pub fn get_or_default(&self, name: Option<&str>) -> Arc<Personality> {
        let guard = self.inner.read().unwrap();
        name.and_then(|n| guard.get(n)).or_else(|| guard.get("default")).cloned().unwrap()
    }
    pub fn list(&self) -> Vec<Arc<Personality>> {
        let mut v: Vec<_> = self.inner.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }
}
```

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test personalities::`
Expected: PASS (3 tests). Add `pub mod personalities;` to `src/lib.rs`.

- [ ] **Step 7: Commit**

```bash
git add src/personalities.rs personalities/ src/lib.rs
git commit -m "feat: personality TOML loader with required default and skip-on-error"
```

---

## Phase 3 — LLM (Ollama)

### Task 5: LlmBackend trait + relevance JSON parsing + MockLlm

**Files:**
- Create: `src/llm.rs`
- Modify: `src/lib.rs` (add `pub mod llm;`)

**Interfaces:**
- Produces:
  - `pub struct ChatRequest { model: String, system: String, turns: Vec<ChatTurn>, temperature: f32, top_p: f32, num_ctx: u32 }`
  - `pub struct Relevance { should_reply: bool, confidence: f32 }`
  - `#[async_trait] pub trait LlmBackend: Send + Sync { async fn generate_reply(&self, req: ChatRequest) -> anyhow::Result<String>; async fn relevance_check(&self, model: &str, num_ctx: u32, turns: Vec<ChatTurn>) -> anyhow::Result<Relevance>; }`
  - `pub fn parse_relevance(s: &str) -> Relevance` (tolerant: extracts the first JSON object; defaults to `{false, 0.0}` on parse failure)
  - `pub struct MockLlm { pub reply: String, pub relevance: Relevance }` implementing the trait

- [ ] **Step 1: Write the failing test** — append to `src/llm.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let r = parse_relevance(r#"{"should_reply": true, "confidence": 0.82}"#);
        assert!(r.should_reply); assert!((r.confidence - 0.82).abs() < 1e-6);
    }

    #[test]
    fn extracts_json_from_noise() {
        let r = parse_relevance("Sure!\n{\"should_reply\": false, \"confidence\": 0.1} \n");
        assert!(!r.should_reply);
    }

    #[test]
    fn junk_defaults_to_silent() {
        let r = parse_relevance("no idea");
        assert!(!r.should_reply); assert_eq!(r.confidence, 0.0);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test llm::`
Expected: FAIL — `parse_relevance` not found.

- [ ] **Step 3: Write minimal implementation** — top of `src/llm.rs`:

```rust
use crate::types::{ChatTurn, Role};
use async_trait::async_trait;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String, pub system: String, pub turns: Vec<ChatTurn>,
    pub temperature: f32, pub top_p: f32, pub num_ctx: u32,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Relevance { pub should_reply: bool, pub confidence: f32 }

pub fn parse_relevance(s: &str) -> Relevance {
    let (start, end) = (s.find('{'), s.rfind('}'));
    if let (Some(a), Some(b)) = (start, end) {
        if b > a {
            if let Ok(r) = serde_json::from_str::<Relevance>(&s[a..=b]) { return r; }
        }
    }
    Relevance { should_reply: false, confidence: 0.0 }
}

#[async_trait]
pub trait LlmBackend: Send + Sync {
    async fn generate_reply(&self, req: ChatRequest) -> anyhow::Result<String>;
    async fn relevance_check(&self, model: &str, num_ctx: u32, turns: Vec<ChatTurn>) -> anyhow::Result<Relevance>;
}

pub struct MockLlm { pub reply: String, pub relevance: Relevance }
#[async_trait]
impl LlmBackend for MockLlm {
    async fn generate_reply(&self, _req: ChatRequest) -> anyhow::Result<String> { Ok(self.reply.clone()) }
    async fn relevance_check(&self, _m: &str, _c: u32, _t: Vec<ChatTurn>) -> anyhow::Result<Relevance> { Ok(self.relevance) }
}

// helper used by both real client and prompt building
pub(crate) fn turn_json(t: &ChatTurn) -> serde_json::Value {
    let content = match &t.name {
        Some(n) if t.role == Role::User => format!("{n}: {}", t.content),
        _ => t.content.clone(),
    };
    serde_json::json!({ "role": t.role.as_str(), "content": content })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test llm::`
Expected: PASS. Add `pub mod llm;` to `src/lib.rs`.

- [ ] **Step 5: Commit**

```bash
git add src/llm.rs src/lib.rs
git commit -m "feat: LlmBackend trait, tolerant relevance JSON parser, MockLlm"
```

### Task 6: OllamaClient (real backend)

**Files:**
- Modify: `src/llm.rs`

**Interfaces:**
- Produces: `pub struct OllamaClient { base_url: String, http: reqwest::Client }` with `OllamaClient::new(base_url: impl Into<String>) -> Self` implementing `LlmBackend`. Uses Ollama `POST /api/chat` with `"stream": false`, `options: {temperature, top_p, num_ctx}`. `relevance_check` prepends a fixed system turn asking for strict JSON and low `num_predict`.

- [ ] **Step 1: Write implementation** (network call; verified by the integration smoke test in Task 18, not a unit test) — add to `src/llm.rs`:

```rust
pub struct OllamaClient { base_url: String, http: reqwest::Client }

impl OllamaClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build().expect("reqwest client"),
        }
    }

    async fn chat(&self, model: &str, msgs: Vec<serde_json::Value>, opts: serde_json::Value)
        -> anyhow::Result<String>
    {
        let body = serde_json::json!({ "model": model, "messages": msgs, "stream": false, "options": opts });
        let resp = self.http.post(format!("{}/api/chat", self.base_url))
            .json(&body).send().await?.error_for_status()?;
        let v: serde_json::Value = resp.json().await?;
        Ok(v["message"]["content"].as_str().unwrap_or_default().trim().to_string())
    }
}

const RELEVANCE_SYS: &str = "You decide whether the assistant should chime in UNPROMPTED to a group chat. \
Reply with ONLY a JSON object: {\"should_reply\": bool, \"confidence\": number 0..1}. \
Set should_reply true only if the assistant can add clear value right now.";

#[async_trait]
impl LlmBackend for OllamaClient {
    async fn generate_reply(&self, req: ChatRequest) -> anyhow::Result<String> {
        let mut msgs = vec![serde_json::json!({"role":"system","content": req.system})];
        msgs.extend(req.turns.iter().map(turn_json));
        let opts = serde_json::json!({"temperature": req.temperature, "top_p": req.top_p, "num_ctx": req.num_ctx});
        self.chat(&req.model, msgs, opts).await
    }

    async fn relevance_check(&self, model: &str, num_ctx: u32, turns: Vec<ChatTurn>) -> anyhow::Result<Relevance> {
        let mut msgs = vec![serde_json::json!({"role":"system","content": RELEVANCE_SYS})];
        msgs.extend(turns.iter().map(turn_json));
        let opts = serde_json::json!({"temperature": 0.0, "num_ctx": num_ctx, "num_predict": 40});
        let raw = self.chat(model, msgs, opts).await?;
        Ok(parse_relevance(&raw))
    }
}
```

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: compiles.

- [ ] **Step 3: Commit**

```bash
git add src/llm.rs
git commit -m "feat: OllamaClient implementing LlmBackend (chat + relevance)"
```

---

## Phase 4 — Signal transport

### Task 7: SignalTransport trait + MockSignal

**Files:**
- Create: `src/signal.rs`
- Modify: `src/lib.rs` (add `pub mod signal;`)

**Interfaces:**
- Produces:
  - `#[async_trait] pub trait SignalTransport: Send + Sync { async fn send(&self, room_id: &str, is_group: bool, text: &str) -> anyhow::Result<()>; }`
  - `pub struct MockSignal { pub sent: Arc<Mutex<Vec<(String, String)>>> }` recording `(room_id, text)`; `MockSignal::new()`.

- [ ] **Step 1: Write the failing test** — append to `src/signal.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_records_sends() {
        let m = MockSignal::new();
        m.send("g1", true, "hello").await.unwrap();
        assert_eq!(m.sent.lock().unwrap().clone(), vec![("g1".to_string(), "hello".to_string())]);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test signal::`
Expected: FAIL — `MockSignal` not found.

- [ ] **Step 3: Write minimal implementation** — top of `src/signal.rs`:

```rust
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

#[async_trait]
pub trait SignalTransport: Send + Sync {
    async fn send(&self, room_id: &str, is_group: bool, text: &str) -> anyhow::Result<()>;
}

#[derive(Clone, Default)]
pub struct MockSignal { pub sent: Arc<Mutex<Vec<(String, String)>>> }
impl MockSignal { pub fn new() -> Self { Self::default() } }

#[async_trait]
impl SignalTransport for MockSignal {
    async fn send(&self, room_id: &str, _is_group: bool, text: &str) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push((room_id.to_string(), text.to_string()));
        Ok(())
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test signal::`
Expected: PASS. Add `pub mod signal;` to `src/lib.rs`.

- [ ] **Step 5: Commit**

```bash
git add src/signal.rs src/lib.rs
git commit -m "feat: SignalTransport trait and MockSignal"
```

### Task 8: SignalCli — JSON-RPC child process (real transport + receiver)

**Files:**
- Modify: `src/signal.rs`

**Interfaces:**
- Produces:
  - `pub struct SignalCli { account: String, socket: PathBuf, /* rpc write half */ }`
  - `SignalCli::spawn(bin: &str, account: &str, socket: &Path, data_dir: &Path) -> anyhow::Result<(SignalCli, mpsc::Receiver<IncomingMessage>)>` — launches `signal-cli -a <account> --config <data_dir> daemon --socket <socket>` (JSON-RPC over UNIX socket), connects, spawns a read task that parses `receive` notifications into `IncomingMessage` and forwards them; restarts the child with backoff on exit.
  - Implements `SignalTransport::send` via JSON-RPC `send` (params: `groupId` for groups, `recipient` for 1:1).
  - `fn parse_envelope(v: &serde_json::Value, bot_id: &str) -> Option<IncomingMessage>` — pure, unit-tested.

- [ ] **Step 1: Write the failing test** (pure parser only) — add to `signal.rs` `tests`:

```rust
    use crate::types::IncomingMessage;

    #[test]
    fn parses_group_data_message() {
        let v: serde_json::Value = serde_json::from_str(r#"{
          "method":"receive","params":{"envelope":{
            "source":"+1000","sourceName":"Alice","timestamp":1710000000000,
            "dataMessage":{"message":"hey @bot","mentions":[{"number":"+15555550100"}],
              "groupInfo":{"groupId":"GID=="}}}}}"#).unwrap();
        let m = parse_envelope(&v, "+15555550100").unwrap();
        assert_eq!(m.room_id, "GID==");
        assert!(m.is_group);
        assert!(m.is_mention);
        assert_eq!(m.sender_id, "+1000");
        assert_eq!(m.body, "hey @bot");
    }

    #[test]
    fn direct_message_room_is_sender() {
        let v: serde_json::Value = serde_json::from_str(r#"{
          "method":"receive","params":{"envelope":{
            "source":"+1000","sourceName":"Alice","timestamp":1,
            "dataMessage":{"message":"hi"}}}}"#).unwrap();
        let m = parse_envelope(&v, "+15555550100").unwrap();
        assert_eq!(m.room_id, "+1000");
        assert!(!m.is_group);
        assert!(!m.is_mention);
    }

    #[test]
    fn non_data_message_is_none() {
        let v: serde_json::Value = serde_json::from_str(r#"{"method":"receive","params":{"envelope":{"source":"+1","receiptMessage":{}}}}"#).unwrap();
        assert!(parse_envelope(&v, "+15555550100").is_none());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test signal::parses_group`
Expected: FAIL — `parse_envelope` not found.

- [ ] **Step 3: Write the parser** — add to `src/signal.rs`:

```rust
use crate::types::IncomingMessage;

pub fn parse_envelope(v: &serde_json::Value, bot_id: &str) -> Option<IncomingMessage> {
    let env = v.get("params")?.get("envelope")?;
    let dm = env.get("dataMessage")?;
    let body = dm.get("message")?.as_str()?.to_string();
    let source = env.get("source")?.as_str()?.to_string();
    let source_name = env.get("sourceName").and_then(|x| x.as_str()).map(String::from);
    let ts = env.get("timestamp").and_then(|x| x.as_i64()).unwrap_or(0);
    let group_id = dm.get("groupInfo").and_then(|g| g.get("groupId")).and_then(|x| x.as_str());
    let is_group = group_id.is_some();
    let room_id = group_id.map(String::from).unwrap_or_else(|| source.clone());
    let is_mention = dm.get("mentions").and_then(|m| m.as_array())
        .map(|arr| arr.iter().any(|m| m.get("number").and_then(|n| n.as_str()) == Some(bot_id)))
        .unwrap_or(false);
    let quoted_msg = dm.get("quote").and_then(|q| q.get("text")).and_then(|x| x.as_str()).map(String::from);
    Some(IncomingMessage { room_id, sender_id: source, sender_name: source_name, body,
        is_group, is_mention, quoted_msg, timestamp: ts })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test signal::`
Expected: PASS (parser tests + MockSignal).

- [ ] **Step 5: Write `SignalCli` spawn/send** — add to `src/signal.rs` (uses `tokio::net::UnixStream`, `tokio::process::Command`, `tokio::sync::mpsc`; newline-delimited JSON-RPC). Implement:
  - `spawn(...)`: start child `daemon --socket`, wait for socket file, connect `UnixStream`, split; reader task loops reading lines, `serde_json::from_str`, `parse_envelope`, `tx.send`. On child exit or socket error, log and respawn with capped exponential backoff (1s→30s). Return `(SignalCli, rx)`.
  - `SignalTransport for SignalCli`: build JSON-RPC request `{"jsonrpc":"2.0","id":<n>,"method":"send","params":{...}}` with `groupId` or `recipient`, write line to the socket.

```rust
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex as AsyncMutex};

pub struct SignalCli {
    account: String,
    writer: Arc<AsyncMutex<tokio::net::unix::OwnedWriteHalf>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl SignalCli {
    pub async fn spawn(bin: &str, account: &str, socket: &Path, data_dir: &Path)
        -> anyhow::Result<(Arc<SignalCli>, mpsc::Receiver<IncomingMessage>)>
    {
        // (implementation: spawn child, connect UnixStream, split, reader task with respawn/backoff)
        // returns the client (send half) and the receiver stream of IncomingMessage
        unimplemented!("fill per Step 5 description")
    }
}

#[async_trait]
impl SignalTransport for SignalCli {
    async fn send(&self, room_id: &str, is_group: bool, text: &str) -> anyhow::Result<()> {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let params = if is_group {
            serde_json::json!({"groupId": room_id, "message": text})
        } else {
            serde_json::json!({"recipient": [room_id], "message": text})
        };
        let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"send","params":params});
        let mut line = serde_json::to_string(&req)?; line.push('\n');
        self.writer.lock().await.write_all(line.as_bytes()).await?;
        Ok(())
    }
}
```

> Note: the `spawn` body is the one place a code block is a described skeleton — implement it against the Step-5 bullet list. Everything it needs (`parse_envelope`, backoff, channel) is defined above. Verify manually against a live daemon in Task 18; the parser and send-formatting are unit/inspection covered.

- [ ] **Step 6: Build + test**

Run: `cargo build && cargo test signal::`
Expected: compiles; parser + mock tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/signal.rs
git commit -m "feat: SignalCli JSON-RPC child (envelope parser, send, supervised reader)"
```

---

## Phase 5 — Router (policy + orchestration)

### Task 9: Reply decision (addressed/always) + rate limiter

**Files:**
- Create: `src/router.rs`
- Modify: `src/lib.rs` (add `pub mod router;`)

**Interfaces:**
- Produces:
  - `pub enum Decision { Reply, Proactive, Silent }`
  - `pub fn decide(room: &Room, msg: &IncomingMessage) -> Decision` — pure: 1:1 → Reply; group addressed-mode → Reply if `is_mention` or `quoted_msg.is_some()` else Silent; always-mode → Reply; proactive-mode → Proactive (gate applied later).
  - `pub struct RateLimiter` with `fn allow(&self, room: &str, cooldown_secs: u64, max_per_hour: u32, now: Instant/epoch) -> bool` (in-memory; injectable clock via passing `now_secs: u64`).

- [ ] **Step 1: Write the failing test** — append to `src/router.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test router::`
Expected: FAIL — `decide` / `RateLimiter` not found.

- [ ] **Step 3: Write minimal implementation** — top of `src/router.rs`:

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test router::`
Expected: PASS. Add `pub mod router;` to `src/lib.rs`.

- [ ] **Step 5: Commit**

```bash
git add src/router.rs src/lib.rs
git commit -m "feat: reply decision policy and per-room proactive rate limiter"
```

### Task 10: Prompt building from window

**Files:**
- Modify: `src/router.rs`

**Interfaces:**
- Produces: `pub fn build_turns(window: &[StoredMessage], bot_id: &str) -> Vec<ChatTurn>` — maps stored messages to turns: `role == Assistant` (or `sender_id == bot_id`) → `ChatTurn{Assistant, None, body}`; else `ChatTurn{User, sender_name, body}`. And `pub fn system_prompt(personality: &Personality, room: &Room) -> String` — personality prompt + one line of room context for groups.

- [ ] **Step 1: Write the failing test** — add to `router.rs` `tests`:

```rust
    use crate::types::{StoredMessage, Role, ChatTurn};

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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test router::build_turns`
Expected: FAIL — `build_turns` not found.

- [ ] **Step 3: Write minimal implementation** — add to `src/router.rs`:

```rust
use crate::types::{ChatTurn, Role, StoredMessage};
use crate::personalities::Personality;

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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test router::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/router.rs
git commit -m "feat: prompt construction (turns + system prompt with room context)"
```

### Task 11: Router orchestration (`handle`) end-to-end with mocks

**Files:**
- Modify: `src/router.rs`
- Create: `tests/router_it.rs`

**Interfaces:**
- Produces:
  - `pub struct Router { store: Store, personalities: Arc<Personalities>, llm: Arc<dyn LlmBackend>, signal: Arc<dyn SignalTransport>, bot_id: String, dry_run: bool, rl: RateLimiter }`
  - `Router::new(...) -> Router`
  - `async fn handle(&self, msg: IncomingMessage) -> anyhow::Result<Option<String>>` — records incoming; ensures room; `decide`; for `Proactive`, run `relevance_check` + `rl.allow` (personality thresholds) → skip if failing; on reply, select window (`recent` + `select_window`), build turns, `generate_reply`, record assistant msg (unless dry_run), `signal.send` (unless dry_run). Returns the reply text if one was produced (even in dry-run), else `None`.

- [ ] **Step 1: Write the failing integration test** — create `tests/router_it.rs`:

```rust
use signal_bot::llm::{MockLlm, Relevance};
use signal_bot::personalities::Personalities;
use signal_bot::router::Router;
use signal_bot::signal::MockSignal;
use signal_bot::store::Store;
use signal_bot::types::{IncomingMessage, ReplyMode};
use std::sync::Arc;
use std::io::Write;

fn personalities() -> Arc<Personalities> {
    let d = tempfile::tempdir().unwrap();
    let body = "label=\"D\"\nsystem_prompt=\"be nice\"\nmodel=\"m\"\ntemperature=0.5\ntop_p=0.9\nnum_ctx=4096\n[proactive]\nrelevance_threshold=0.7\ncooldown_secs=60\nmax_per_hour=5\n";
    std::fs::File::create(d.path().join("default.toml")).unwrap().write_all(body.as_bytes()).unwrap();
    let p = Arc::new(Personalities::load_dir(d.path()).unwrap());
    std::mem::forget(d); // keep temp dir alive for the test process
    p
}

fn incoming(room: &str, group: bool, mention: bool) -> IncomingMessage {
    IncomingMessage { room_id: room.into(), sender_id: "+1000".into(), sender_name: Some("Alice".into()),
        body: "hello bot".into(), is_group: group, is_mention: mention, quoted_msg: None, timestamp: 1 }
}

#[tokio::test]
async fn direct_message_gets_reply_and_is_sent() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "hi there".into(), relevance: Relevance{should_reply:false, confidence:0.0} });
    let r = Router::new(store.clone(), personalities(), llm, sig.clone(), "+bot".into(), false);

    let out = r.handle(incoming("+1000", false, false)).await.unwrap();
    assert_eq!(out.as_deref(), Some("hi there"));
    assert_eq!(sig.sent.lock().unwrap().len(), 1);
    // both the incoming and the assistant reply are stored
    assert_eq!(store.recent("+1000", 10).await.unwrap().len(), 2);
}

#[tokio::test]
async fn group_addressed_silent_without_mention() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "x".into(), relevance: Relevance{should_reply:true, confidence:1.0} });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), false);
    let out = r.handle(incoming("G", true, false)).await.unwrap();
    assert!(out.is_none());
    assert!(sig.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn proactive_below_threshold_stays_silent() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.ensure_room("G", Some("Grp"), true).await.unwrap();
    store.set_reply_mode("G", ReplyMode::Proactive).await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "x".into(), relevance: Relevance{should_reply:true, confidence:0.5} });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), false);
    let out = r.handle(incoming("G", true, false)).await.unwrap();
    assert!(out.is_none()); // 0.5 < 0.7 threshold
    assert!(sig.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn dry_run_produces_reply_but_does_not_send() {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(MockLlm { reply: "would say".into(), relevance: Relevance{should_reply:false, confidence:0.0} });
    let r = Router::new(store, personalities(), llm, sig.clone(), "+bot".into(), true);
    let out = r.handle(incoming("+1000", false, false)).await.unwrap();
    assert_eq!(out.as_deref(), Some("would say"));
    assert!(sig.sent.lock().unwrap().is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test router_it`
Expected: FAIL — `Router::new` / `handle` not found.

- [ ] **Step 3: Write minimal implementation** — add to `src/router.rs`:

```rust
use crate::llm::{ChatRequest, LlmBackend};
use crate::personalities::Personalities;
use crate::signal::SignalTransport;
use crate::store::{NewMessage, Store};
use crate::window::select_window;
use std::sync::Arc;

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
            if !self.rl.allow(&room.room_id, personality.proactive.cooldown_secs, personality.proactive.max_per_hour, now_secs()) {
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test router_it && cargo test router::`
Expected: PASS (4 integration + unit tests).

- [ ] **Step 5: Commit**

```bash
git add src/router.rs tests/router_it.rs
git commit -m "feat: Router.handle end-to-end (decision, proactive gate, dry-run)"
```

---

## Phase 6 — Web dashboard

### Task 12: Config + admin credential (argon2)

**Files:**
- Create: `src/config.rs`
- Modify: `src/store.rs` (admin get/set), `src/lib.rs` (add `pub mod config;`)

**Interfaces:**
- Produces:
  - `src/config.rs`: `pub struct AppConfig { data_dir, personalities_dir, signal_bin, signal_account, ollama_url, bind_addr, cert_path, key_path, debug, dry_run }` loaded from a TOML file + `clap` CLI/env overrides; `pub struct Cli` (clap derive) with `--debug`, `--dry-run`, `--repl`, `--config <path>`.
  - `src/store.rs`: `async fn set_admin(&self, username: &str, password: &str) -> Result<()>` (argon2 hash), `async fn verify_admin(&self, username: &str, password: &str) -> Result<bool>`.

- [ ] **Step 1: Write the failing test** — add to `store.rs` `tests`:

```rust
    #[tokio::test]
    async fn admin_password_roundtrip() {
        let s = mem().await;
        s.set_admin("admin", "s3cret").await.unwrap();
        assert!(s.verify_admin("admin", "s3cret").await.unwrap());
        assert!(!s.verify_admin("admin", "wrong").await.unwrap());
        assert!(!s.verify_admin("nobody", "s3cret").await.unwrap());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test store::admin_password`
Expected: FAIL — `set_admin` not found.

- [ ] **Step 3: Write minimal implementation** — add to `src/store.rs`:

```rust
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{rand_core::OsRng, SaltString};

impl Store {
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
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test store::admin_password`
Expected: PASS.

- [ ] **Step 5: Write `src/config.rs`** (clap derive + TOML load; no test needed, exercised in main):

```rust
use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Parser, Debug)]
pub struct Cli {
    #[arg(long, default_value = "/etc/signal-bot/config.toml")]
    pub config: PathBuf,
    #[arg(long, env = "SIGNAL_BOT_DEBUG")]
    pub debug: bool,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub repl: bool,
    /// Subcommand-less admin bootstrap: `--set-admin user:pass`
    #[arg(long)]
    pub set_admin: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub data_dir: PathBuf,
    pub personalities_dir: PathBuf,
    pub signal_bin: String,
    pub signal_account: String,
    pub ollama_url: String,
    pub bind_addr: String,     // e.g. 0.0.0.0:8443 (LAN)
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    #[serde(default)] pub debug: bool,
    #[serde(default)] pub dry_run: bool,
}

impl AppConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<AppConfig> {
        let text = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&text)?)
    }
    pub fn db_url(&self) -> String { format!("sqlite://{}/bot.sqlite?mode=rwc", self.data_dir.display()) }
    pub fn socket_path(&self) -> PathBuf { self.data_dir.join("signal-cli.sock") }
}
```

- [ ] **Step 6: Commit**

```bash
git add src/store.rs src/config.rs src/lib.rs
git commit -m "feat: argon2 admin credential + AppConfig/CLI"
```

### Task 13: Web app — auth, room list, room detail, assign (axum handlers)

**Files:**
- Create: `src/web/mod.rs`, `src/web/auth.rs`, `src/web/handlers.rs`, `templates/base.html`, `templates/login.html`, `templates/rooms.html`, `templates/room.html`
- Create: `tests/web_it.rs`
- Modify: `src/lib.rs` (add `pub mod web;`)

**Interfaces:**
- Produces:
  - `pub struct AppState { store: Store, personalities: Arc<Personalities>, tx: broadcast::Sender<SseEvent> }`
  - `pub fn build_router(state: AppState) -> axum::Router` — routes: `GET /login`, `POST /login`, `POST /logout`, `GET /` (auth), `GET /rooms/:id` (auth), `POST /rooms/:id/personality` (auth), `POST /rooms/:id/mode` (auth), `GET /events` (auth, SSE), plus static assets.
  - Auth via `tower-sessions`; login rate-limit (simple in-memory per-IP counter).
  - `pub async fn serve_tls(state, bind_addr, cert, key)` — axum-server rustls.

- [ ] **Step 1: Write the failing integration test** — create `tests/web_it.rs` (drive the router with `tower::ServiceExt::oneshot`, no TLS):

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use signal_bot::personalities::Personalities;
use signal_bot::store::Store;
use signal_bot::web::{build_router, AppState};
use std::sync::Arc;
use std::io::Write;
use tower::ServiceExt;

async fn state() -> AppState {
    let store = Store::connect("sqlite::memory:").await.unwrap();
    store.set_admin("admin", "pw").await.unwrap();
    store.ensure_room("G", Some("Grp"), true).await.unwrap();
    let d = tempfile::tempdir().unwrap();
    let body = "label=\"D\"\nsystem_prompt=\"x\"\nmodel=\"m\"\ntemperature=0.5\ntop_p=0.9\nnum_ctx=4096\n[proactive]\nrelevance_threshold=0.7\ncooldown_secs=60\nmax_per_hour=5\n";
    std::fs::File::create(d.path().join("default.toml")).unwrap().write_all(body.as_bytes()).unwrap();
    let p = Arc::new(Personalities::load_dir(d.path()).unwrap());
    std::mem::forget(d);
    AppState::new(store, p)
}

#[tokio::test]
async fn unauthenticated_root_redirects_to_login() {
    let app = build_router(state().await);
    let resp = app.oneshot(Request::builder().uri("/").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER); // 303 -> /login
}

#[tokio::test]
async fn login_page_is_public() {
    let app = build_router(state().await);
    let resp = app.oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test web_it`
Expected: FAIL — `build_router` / `AppState` not found.

- [ ] **Step 3: Write templates** — `templates/base.html` (askama layout with `{% block content %}`), `login.html` (username/password form POST `/login`), `rooms.html` (list rooms with links + current personality/mode badges), `room.html` (message history + `<select>` forms POSTing to `/rooms/:id/personality` and `/rooms/:id/mode` + an SSE `<script>` appending new messages from `/events`). Keep CSS minimal inline in `base.html`.

- [ ] **Step 4: Write handlers + state + auth** — `src/web/mod.rs`, `auth.rs`, `handlers.rs`:
  - `AppState::new(store, personalities)` creates a `tokio::sync::broadcast` channel (`SseEvent { room_id, sender, body }`); expose `state.tx` so the router task can publish new messages.
  - `build_router` wires routes + `tower_sessions::SessionManagerLayer` (cookie: HttpOnly, `Secure`, `SameSite::Strict`).
  - Auth middleware: check session key `admin=true`; if absent on protected routes redirect 303 → `/login`.
  - `POST /login`: rate-limit per IP; `store.verify_admin`; on success set session; redirect `/`.
  - Handlers use askama structs; `set_personality`/`set_mode` validate against `personalities.get(..)` / `ReplyMode::parse` then call store and redirect back.
  - `GET /events`: `axum::response::sse::Sse` from a `BroadcastStream` of `state.tx.subscribe()`.

```rust
// src/web/mod.rs (shape)
use crate::personalities::Personalities;
use crate::store::Store;
use std::sync::Arc;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct SseEvent { pub room_id: String, pub sender: String, pub body: String }

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub personalities: Arc<Personalities>,
    pub tx: broadcast::Sender<SseEvent>,
}
impl AppState {
    pub fn new(store: Store, personalities: Arc<Personalities>) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self { store, personalities, tx }
    }
}

pub fn build_router(state: AppState) -> axum::Router { /* routes per Step 4 */ unimplemented!() }
pub async fn serve_tls(state: AppState, bind: &str, cert: &std::path::Path, key: &std::path::Path) -> anyhow::Result<()> { /* axum-server rustls */ unimplemented!() }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --test web_it`
Expected: PASS (redirect + login-page tests). Add `pub mod web;` to `src/lib.rs`.

- [ ] **Step 6: Add an authenticated-flow test** — append to `tests/web_it.rs`: POST `/login` with correct creds, capture the session cookie, then GET `/` with the cookie → 200; POST `/rooms/G/mode` with `mode=proactive` → 303 and `store.get_room("G").reply_mode == Proactive`. Run `cargo test --test web_it`; expected PASS.

- [ ] **Step 7: Commit**

```bash
git add src/web/ templates/ tests/web_it.rs src/lib.rs
git commit -m "feat: web dashboard (auth, room list/detail, assign personality/mode, SSE)"
```

---

## Phase 7 — Wiring, CLI modes, hot-reload

### Task 14: `main.rs` wiring + task supervision + `--set-admin`

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `AppConfig`, `Cli`, `Store`, `Personalities`, `SignalCli`, `OllamaClient`, `Router`, `web::{AppState, serve_tls}`.
- Produces: a running binary. `--set-admin user:pass` sets the credential and exits. Otherwise: connect store, load personalities, spawn signal, build router, spawn (a) the receive→router loop, (b) the web server, (c) hot-reload watcher; supervise: if a task exits, log and restart with backoff; process stays up.

- [ ] **Step 1: Implement `main.rs`**

```rust
use clap::Parser;
use signal_bot::{config::{AppConfig, Cli}, store::Store, personalities::Personalities};
use signal_bot::llm::OllamaClient;
use signal_bot::signal::SignalCli;
use signal_bot::router::Router;
use signal_bot::web::{self, AppState};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let filter = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into())).init();

    let cfg = AppConfig::load(&cli.config)?;
    let store = Store::connect(&cfg.db_url()).await?;

    if let Some(spec) = cli.set_admin.as_deref() {
        let (u, p) = spec.split_once(':').ok_or_else(|| anyhow::anyhow!("use --set-admin user:pass"))?;
        store.set_admin(u, p).await?;
        println!("admin credential set for '{u}'");
        return Ok(());
    }

    let personalities = Arc::new(Personalities::load_dir(&cfg.personalities_dir)?);
    let dry_run = cli.dry_run || cfg.dry_run;

    // web state + server
    let state = AppState::new(store.clone(), personalities.clone());
    {
        let (bind, cert, key, st) = (cfg.bind_addr.clone(), cfg.cert_path.clone(), cfg.key_path.clone(), state.clone());
        tokio::spawn(async move {
            loop {
                if let Err(e) = web::serve_tls(st.clone(), &bind, &cert, &key).await {
                    tracing::error!("web server exited: {e}; restarting in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        });
    }

    if cli.repl {
        signal_bot::repl::run(store, personalities, dry_run, cfg.ollama_url.clone()).await?;
        return Ok(());
    }

    // real signal + llm + router
    let (signal, mut rx) = SignalCli::spawn(&cfg.signal_bin, &cfg.signal_account, &cfg.socket_path(), &cfg.data_dir).await?;
    let llm = Arc::new(OllamaClient::new(cfg.ollama_url.clone()));
    let router = Arc::new(Router::new(store, personalities.clone(), llm, signal, cfg.signal_account.clone(), dry_run));

    // hot-reload watcher (Task 15) - spawn here
    signal_bot::personalities_watch::spawn(personalities.clone(), cfg.personalities_dir.clone());

    // receive loop
    while let Some(msg) = rx.recv().await {
        let router = router.clone();
        let tx = state.tx.clone();
        tokio::spawn(async move {
            let sender = msg.sender_name.clone().unwrap_or_else(|| msg.sender_id.clone());
            let room_id = msg.room_id.clone();
            let body = msg.body.clone();
            let _ = tx.send(signal_bot::web::SseEvent { room_id: room_id.clone(), sender, body });
            match router.handle(msg).await {
                Ok(Some(reply)) => { let _ = tx.send(signal_bot::web::SseEvent { room_id, sender: "bot".into(), body: reply }); }
                Ok(None) => {}
                Err(e) => tracing::error!("handle error: {e}"),
            }
        });
    }
    Ok(())
}
```

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: compiles once Tasks 8, 13, 15, 16 provide `SignalCli::spawn`, `serve_tls`, `build_router`, watcher, and `repl::run`. (If building this task before those land, stub the missing `unimplemented!()` bodies so it compiles; they are filled in their own tasks.)

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: main wiring, task supervision, --set-admin bootstrap"
```

### Task 15: Personality hot-reload watcher

**Files:**
- Create: `src/personalities_watch.rs`
- Modify: `src/lib.rs` (add `pub mod personalities_watch;`)

**Interfaces:**
- Consumes: `Arc<Personalities>` (has `reload(dir)` from Task 4).
- Produces: `pub fn spawn(p: Arc<Personalities>, dir: PathBuf)` — uses `notify` to watch `dir`; on any change, debounce 500ms then `p.reload(&dir)`, logging success/failure. Also install a SIGHUP handler (unix) that triggers a reload.

- [ ] **Step 1: Write implementation**

```rust
use crate::personalities::Personalities;
use std::path::PathBuf;
use std::sync::Arc;

pub fn spawn(p: Arc<Personalities>, dir: PathBuf) {
    // notify watcher
    let (p1, d1) = (p.clone(), dir.clone());
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};
        let (tx, rx) = std::sync::mpsc::channel();
        let mut w = match notify::recommended_watcher(tx) { Ok(w) => w, Err(e) => { tracing::error!("watcher: {e}"); return; } };
        if let Err(e) = w.watch(&d1, RecursiveMode::NonRecursive) { tracing::error!("watch: {e}"); return; }
        loop {
            match rx.recv() {
                Ok(_) => {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    while rx.try_recv().is_ok() {}
                    match p1.reload(&d1) { Ok(_) => tracing::info!("personalities reloaded"), Err(e) => tracing::error!("reload: {e}") }
                }
                Err(_) => break,
            }
        }
    });

    // SIGHUP
    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut hup = match signal(SignalKind::hangup()) { Ok(s) => s, Err(e) => { tracing::error!("sighup: {e}"); return; } };
        while hup.recv().await.is_some() {
            match p.reload(&dir) { Ok(_) => tracing::info!("personalities reloaded (SIGHUP)"), Err(e) => tracing::error!("reload: {e}") }
        }
    });
}
```

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: compiles. Add `pub mod personalities_watch;` to `src/lib.rs`.

- [ ] **Step 3: Commit**

```bash
git add src/personalities_watch.rs src/lib.rs
git commit -m "feat: personality hot-reload via notify + SIGHUP"
```

### Task 16: `--repl` debug harness

**Files:**
- Create: `src/repl.rs`
- Modify: `src/lib.rs` (add `pub mod repl;`)

**Interfaces:**
- Consumes: `Store`, `Arc<Personalities>`, `OllamaClient` (via url), `MockSignal`, `Router`.
- Produces: `pub async fn run(store: Store, personalities: Arc<Personalities>, dry_run: bool, ollama_url: String) -> anyhow::Result<()>` — reads lines from stdin in the form `room|group?|mention?|sender|text` (e.g. `G|1|1|Alice|hey bot`), builds an `IncomingMessage`, calls `Router::handle` with a `MockSignal`, and prints the reply (or "[silent]"). Uses the **real** OllamaClient so you can test prompts/personalities against the live model without Signal.

- [ ] **Step 1: Write implementation**

```rust
use crate::llm::OllamaClient;
use crate::personalities::Personalities;
use crate::router::Router;
use crate::signal::MockSignal;
use crate::store::Store;
use crate::types::IncomingMessage;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

pub async fn run(store: Store, personalities: Arc<Personalities>, dry_run: bool, ollama_url: String) -> anyhow::Result<()> {
    let sig = Arc::new(MockSignal::new());
    let llm = Arc::new(OllamaClient::new(ollama_url));
    let router = Router::new(store, personalities, llm, sig, "+bot".into(), dry_run);
    eprintln!("REPL: room|group(0/1)|mention(0/1)|sender|text  (Ctrl-D to exit)");
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let parts: Vec<&str> = line.splitn(5, '|').collect();
        if parts.len() != 5 { eprintln!("bad format"); continue; }
        let msg = IncomingMessage {
            room_id: parts[0].into(), is_group: parts[1] == "1", is_mention: parts[2] == "1",
            sender_id: parts[3].into(), sender_name: Some(parts[3].into()),
            body: parts[4].into(), quoted_msg: None, timestamp: 0,
        };
        match router.handle(msg).await {
            Ok(Some(reply)) => println!("BOT> {reply}"),
            Ok(None) => println!("[silent]"),
            Err(e) => eprintln!("error: {e}"),
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Build**

Run: `cargo build`
Expected: compiles. Add `pub mod repl;` to `src/lib.rs`.

- [ ] **Step 3: Commit**

```bash
git add src/repl.rs src/lib.rs
git commit -m "feat: --repl harness for testing routing/LLM without Signal"
```

---

## Phase 8 — Deployment & registration

### Task 17: TLS cert generation + config template + systemd unit + setup script

**Files:**
- Create: `deploy/gen-cert.sh`, `deploy/config.example.toml`, `deploy/signal-bot.service`, `deploy/setup.sh`, `deploy/REGISTER.md`

**Interfaces:**
- Produces: everything the `bot` admin runs on bot.local to provision, plus the manual Signal registration doc.

- [ ] **Step 1: `deploy/config.example.toml`**

```toml
data_dir = "/var/lib/signal-bot"
personalities_dir = "/etc/signal-bot/personalities"
signal_bin = "/usr/local/bin/signal-cli"
signal_account = "+15555550100"
ollama_url = "http://127.0.0.1:11434"
bind_addr = "0.0.0.0:8443"          # LAN interface; firewall to the LAN
cert_path = "/etc/signal-bot/tls/bot.local.crt"
key_path  = "/etc/signal-bot/tls/bot.local.key"
debug = false
dry_run = false
```

- [ ] **Step 2: `deploy/gen-cert.sh`** — generate a local CA + `bot.local` leaf cert (prefer `mkcert` if installed; else `openssl` with SAN `DNS:bot.local`). Write cert/key to `/etc/signal-bot/tls/`, `chmod 640`, `chown root:signal-bot`. Print the CA path to import into viewing devices' trust stores.

```bash
#!/usr/bin/env bash
set -euo pipefail
TLS=/etc/signal-bot/tls
sudo mkdir -p "$TLS"
if command -v mkcert >/dev/null; then
  mkcert -cert-file "$TLS/bot.local.crt" -key-file "$TLS/bot.local.key" bot.local
  echo "Import this CA on viewing devices: $(mkcert -CAROOT)/rootCA.pem"
else
  sudo openssl req -x509 -newkey rsa:4096 -sha256 -days 3650 -nodes \
    -keyout "$TLS/bot.local.key" -out "$TLS/bot.local.crt" \
    -subj "/CN=bot.local" -addext "subjectAltName=DNS:bot.local"
  echo "Self-signed cert created; trust $TLS/bot.local.crt on viewing devices."
fi
sudo chown root:signal-bot "$TLS"/bot.local.*
sudo chmod 640 "$TLS"/bot.local.*
```

- [ ] **Step 3: `deploy/signal-bot.service`** — hardened systemd unit running as `signal-bot`.

```ini
[Unit]
Description=Signal chat bot
After=network-online.target ollama.service
Wants=network-online.target

[Service]
Type=simple
User=signal-bot
Group=signal-bot
ExecStart=/opt/signal-bot/signal-bot --config /etc/signal-bot/config.toml
Restart=on-failure
RestartSec=5
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/signal-bot
StateDirectory=signal-bot
AmbientCapabilities=
CapabilityBoundingSet=

[Install]
WantedBy=multi-user.target
```

- [ ] **Step 4: `deploy/setup.sh`** — the provisioning script (run by `bot` admin with sudo):

```bash
#!/usr/bin/env bash
set -euo pipefail
# 1. dependencies
command -v java >/dev/null || sudo apt-get install -y default-jre
command -v ollama >/dev/null || curl -fsSL https://ollama.com/install.sh | sh
ollama pull qwen3:8b
# signal-cli: install a pinned release to /usr/local/bin (see REGISTER.md for the exact version/URL)
# 2. service user (system, nologin, no sudo)
id signal-bot &>/dev/null || sudo useradd --system --home /var/lib/signal-bot --shell /usr/sbin/nologin signal-bot
sudo install -d -o signal-bot -g signal-bot -m 0700 /var/lib/signal-bot
sudo install -d -m 0755 /etc/signal-bot /etc/signal-bot/personalities /etc/signal-bot/tls /opt/signal-bot
# 3. config + personalities + binary
sudo install -m 0640 -o root -g signal-bot deploy/config.example.toml /etc/signal-bot/config.toml
sudo cp personalities/*.toml /etc/signal-bot/personalities/
sudo install -m 0755 target/release/signal-bot /opt/signal-bot/signal-bot
# 4. TLS
./deploy/gen-cert.sh
# 5. admin credential (prompts)
read -rp "Web admin username: " U; read -rsp "Web admin password: " P; echo
sudo -u signal-bot /opt/signal-bot/signal-bot --config /etc/signal-bot/config.toml --set-admin "$U:$P"
# 6. enable service (AFTER Signal registration in REGISTER.md)
sudo install -m 0644 deploy/signal-bot.service /etc/systemd/system/signal-bot.service
sudo systemctl daemon-reload
echo "Now complete deploy/REGISTER.md, then: sudo systemctl enable --now signal-bot"
```

- [ ] **Step 5: `deploy/REGISTER.md`** — document the manual Signal registration:

```markdown
# Register the bot's Signal account (run once, as the signal-bot user)

signal-cli data lives in /var/lib/signal-bot (config dir passed via --config).

1. Register (may require solving a captcha; follow the printed link):
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +15555550100 register
   # If prompted for captcha:
   # sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +15555550100 register --captcha "<token>"
2. You will receive an SMS/voice code on the Google Voice number.
3. Verify:
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +15555550100 verify <CODE>
4. Smoke test (send yourself a message):
   sudo -u signal-bot signal-cli --config /var/lib/signal-bot -a +15555550100 send -m "hello" <YOUR_NUMBER>
5. Start the bot: sudo systemctl enable --now signal-bot

Notes:
- The number must NOT already be registered to Signal on another device.
- The daemon uses the same --config data dir the systemd unit passes.
```

- [ ] **Step 6: Commit**

```bash
git add deploy/
git commit -m "feat: deployment (TLS, systemd hardening, setup script, registration doc)"
```

### Task 18: On-host integration smoke test + README

**Files:**
- Create: `README.md`

**Interfaces:** none (docs + manual verification).

- [ ] **Step 1: Write `README.md`** — quickstart: build (`cargo build --release`), run `deploy/setup.sh`, complete `deploy/REGISTER.md`, open `https://bot.local:8443`, trust the cert, log in, assign a personality/mode to a room. Document `--debug`, `--dry-run`, `--repl`, and how to add a personality (drop a TOML in `/etc/signal-bot/personalities/`, hot-reloaded).

- [ ] **Step 2: On-host manual smoke test** (documented checklist, run on bot.local after setup):
  1. `curl -sS http://127.0.0.1:11434/api/tags | grep qwen3` → model present.
  2. `sudo -u signal-bot /opt/signal-bot/signal-bot --config /etc/signal-bot/config.toml --repl --debug` then type `+me|0|0|Me|hello` → observe a `BOT>` reply (verifies store+personalities+Ollama+router without Signal).
  3. `systemctl start signal-bot`; from another Signal account, DM the bot → get a reply; check the dashboard shows the exchange live (SSE).
  4. In a group, set mode `addressed`, @-mention the bot → reply; set `proactive`, send an on-topic message → observe relevance-gated behavior in `journalctl -u signal-bot`.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: README quickstart and on-host smoke-test checklist"
```

---

## Notes for the executor

- Tasks 0–11 have no external dependencies and are fully unit/integration testable in CI (in-memory SQLite, mocks) — do these first and keep them green.
- Task 14 (`main.rs`) references items produced by Tasks 8, 13, 15, 16; if you implement strictly in order, the `unimplemented!()` skeletons from those tasks keep the build compiling until each is filled. Prefer implementing 8/13/15/16 before wiring 14 fully.
- Tasks 17–18 are host-side (bot.local) and verified manually, not in CI.
- Run `cargo test` and `cargo clippy` before every commit; keep the tree green.
