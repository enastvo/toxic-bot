# Signal Chat Bot — Design Spec

**Date:** 2026-09-19
**Status:** Approved design, pre-implementation
**Author:** estefan.nastvogel@icloud.com (with Claude)

## 1. Overview

A self-hosted Signal chat bot written in Rust. It participates in Signal
1:1 and group conversations, replies using a locally-hosted LLM
(`qwen3:8b` via Ollama), and can wear a different **personality** in each
room. A LAN-only, TLS-protected, login-gated web dashboard shows chat
history per room and lets an admin assign which personality and reply
mode is active in each room.

Everything runs on a single host (`bot@bot.local`) as one supervised
Rust process. No inference or chat data leaves the host except Signal's
own transport traffic and one-time package/model downloads.

### Goals

- One Signal identity (Google Voice number `+14433996053`) that behaves
  differently per room via configurable personalities.
- Fully local LLM inference (`qwen3:8b` on Ollama), no external LLM API,
  no per-token cost.
- Per-room reply behavior: addressed-only, always, or proactive
  (relevance-gated unprompted contributions).
- Rolling-window conversation memory per room.
- A read + assign web dashboard (view history, see/set active personality
  and reply mode).
- A debug/test mode that exercises the full pipeline without Signal or a
  live LLM.
- A minimal, local-only attack surface.

### Non-goals (v1)

- Editing personality prompts from the web UI (personalities are TOML
  files on disk).
- Multiple Signal identities / numbers.
- Public/internet exposure of the dashboard.
- Long-term memory beyond the rolling window (no summarization).
- Message-retention/pruning policy (history kept indefinitely in v1).

## 2. Architecture

A single Rust binary (`signal-bot`) running under systemd on bot.local,
structured as async tasks in one `tokio` runtime.

```
                          bot.local (systemd: signal-bot.service)
┌─────────────────────────────────────────────────────────────────────┐
│                                                                       │
│   signal-cli (child proc)            ┌──────────── signal-bot ───────┐│
│   daemon --jsonrpc                   │                                ││
│   over UNIX socket   <──────────────►│  Signal I/O task               ││
│                                      │     │                          ││
│                                      │     ▼                          ││
│                                      │  Router / policy               ││
│                                      │  (per-room reply mode,         ││
│                                      │   relevance gate, cooldown)    ││
│                                      │     │                          ││
│                                      │     ▼                          ││
│   Ollama (127.0.0.1:11434) <────────►│  LLM client (qwen3:8b)         ││
│                                      │     │                          ││
│                                      │     ▼                          ││
│                                      │  Store (SQLite via sqlx)  ◄──┐  ││
│                                      │     ▲                       │  ││
│   Personalities (*.toml) ───────────►│  Config/personality loader │  ││
│                                      │                            │  ││
│   Browser (LAN, HTTPS) <────────────►│  Web task (axum + rustls) ─┘  ││
│                                      └────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────────┘
```

### Key architectural decisions

- **signal-cli in `--jsonrpc` daemon mode over a UNIX socket.** No TCP
  listener; local-only. The bot supervises the child process and
  restarts it on failure with backoff.
- **Ollama bound to localhost** (`127.0.0.1:11434`). The LLM is never
  network-exposed.
- **The only inbound network listener is the axum HTTPS port**, bound to
  the LAN interface and login-gated.
- **One SQLite database (WAL mode)** shared by the router (writer) and
  web task (reader).
- **Task communication via `tokio` channels**; no internal HTTP.
- **`web` and `router` never call each other** — they coordinate only
  through `store`, so a dashboard problem cannot disrupt message
  handling.

### Rejected alternatives

- **signal-cli-rest-api container (Approach B):** adds a third-party
  service, Docker, and a larger supply-chain/attack surface for no gain
  on a single-user host.
- **Split bot + web services (Approach C):** more isolation but more ops
  overhead; YAGNI for a home deployment. Can be split later.

## 3. Components

Each is a focused module with a narrow interface, independently testable.

### 3.1 `signal` (Signal I/O)
- Spawns and supervises the `signal-cli --jsonrpc` child; reconnects and
  restarts on failure with exponential backoff.
- Speaks JSON-RPC over the UNIX socket: subscribes to incoming envelopes,
  exposes `send(room, text)`.
- Normalizes raw envelopes into
  `IncomingMessage { room_id, sender, body, is_mention, quoted_msg, timestamp }`.
- Edge component; no internal dependencies. Emits messages onto a channel.
- Defined behind a `SignalTransport` trait (real + mock impls).

### 3.2 `store` (persistence)
- Owns the SQLite pool (WAL). All SQL behind typed methods:
  `record_message`, `recent_window(room, budget)`, `get_room(room)`,
  `set_room_personality(room, name)`, `set_reply_mode(room, mode)`,
  `list_rooms`, `ensure_room(room, meta)`.
- No internal dependencies. Consumed by `router` and `web`.

### 3.3 `personalities` (config loader)
- Loads `personalities/*.toml` at startup; validates; hot-reloads on file
  change or SIGHUP.
- Exposes `get(name)` and `list()`.
- A `default` personality is required; failure to load it is fatal at
  startup. Other invalid files are logged and skipped.
- No internal dependencies.

### 3.4 `llm` (Ollama client)
- Wraps Ollama's HTTP chat API. Two calls:
  - `generate_reply(system, messages, params) -> String`
  - `relevance_check(system, recent) -> {should_reply: bool, confidence: f32}`
    (proactive mode only)
- Handles timeouts, retries, and Ollama-unavailable errors gracefully.
- No internal dependencies. Defined behind an `LlmBackend` trait (real +
  mock impls).

### 3.5 `router` (policy + orchestration)
- Consumes `IncomingMessage`s. For each: record to `store`; resolve room
  reply mode + active personality; decide whether to reply:
  - **addressed** → reply if mention/quote or 1:1
  - **always** → reply
  - **proactive** → check cooldown & hourly cap, then `relevance_check`;
    reply only if confidence ≥ personality threshold
- On decision to reply: pull `recent_window`, build the prompt from the
  personality + window, call `generate_reply`, send via `signal`, record
  the bot's own message.
- Enforces per-room cooldown/rate limits in memory.
- Depends on `store`, `personalities`, `llm`, `signal`.

### 3.6 `web` (dashboard + assign)
- axum + rustls (TLS). Login-gated session (single admin credential,
  argon2-hashed).
- Read endpoints: list rooms, view a room's history, show active
  personality + reply mode, show Signal/Ollama health status.
- Write endpoints: set a room's active personality (from the loaded set),
  set its reply mode.
- Live updates via SSE so open dashboards refresh as messages arrive.
- Depends on `store`, `personalities` (read-only).

**Boundary check:** `router` is the only component aware of the others;
every other module is a leaf with a narrow interface.

## 4. Data model

SQLite, managed by `sqlx` migrations.

```sql
CREATE TABLE rooms (
    room_id        TEXT PRIMARY KEY,   -- signal group id, or contact id for 1:1
    display_name   TEXT,               -- group title / contact name, best-effort
    is_group       INTEGER NOT NULL,
    personality    TEXT,               -- active personality name; NULL = default
    reply_mode     TEXT NOT NULL DEFAULT 'addressed',  -- addressed|always|proactive
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE messages (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    room_id        TEXT NOT NULL REFERENCES rooms(room_id),
    sender_id      TEXT NOT NULL,      -- signal id; bot's own id for its replies
    sender_name    TEXT,
    role           TEXT NOT NULL,      -- 'user' | 'assistant'
    body           TEXT NOT NULL,
    ts             INTEGER NOT NULL,   -- signal server timestamp (ms)
    personality    TEXT,               -- personality that produced an assistant msg
    is_mention     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_messages_room_ts ON messages(room_id, ts);

CREATE TABLE admin (
    id             INTEGER PRIMARY KEY CHECK (id = 1),
    username       TEXT NOT NULL,
    password_hash  TEXT NOT NULL       -- argon2
);
```

- **Rooms auto-created on first sight** of a message (`ensure_room`); the
  admin then assigns personality/mode from the dashboard.
- The **rolling window is a query** (`recent_window`) over `messages`,
  bounded by a token/count budget — no separate table.
- **History retained indefinitely** in v1.

## 5. Personality format

`personalities/*.toml`, one file per personality (filename = name).

```toml
# personalities/sage.toml
label = "Sage"
description = "Calm, terse technical advisor."

system_prompt = """
You are Sage, a calm and precise technical advisor. Be concise.
Prefer concrete examples. If unsure, say so.
"""

model       = "qwen3:8b"       # optional per-personality override
temperature = 0.4
top_p       = 0.9
num_ctx     = 8192

[proactive]                    # used only when a room is in proactive mode
relevance_threshold = 0.7      # min confidence to speak unprompted
cooldown_secs       = 120      # min seconds between unprompted replies per room
max_per_hour        = 6        # hard cap on unprompted replies per room per hour
```

A `default` personality file is always required; rooms with no assignment
use it.

## 6. Prompt construction

For a reply (in `router`):

1. **System message** = personality `system_prompt`, optionally prefixed
   with light room context (e.g., "You are in a Signal group named X.").
2. **History** = `recent_window(room, num_ctx budget)` mapped to chat
   roles: other people's messages as `user` (prefixed with their display
   name so the model can distinguish speakers in a group), the bot's
   prior messages as `assistant`.
3. **Current message** = the triggering message as the final `user` turn.
4. Sent to Ollama's chat endpoint with the personality's params.

**Proactive relevance gate** = a separate, cheap call: a short system
prompt ("Decide whether this assistant should chime in unprompted. Answer
strictly as JSON `{\"should_reply\": bool, \"confidence\": 0..1}`.") plus
the recent window. Only its JSON verdict is used. Reuses `qwen3:8b` (one
hosted model; the gate prompt is short and fast).

## 7. Error handling

Fail safe; never crash the whole process.

- **signal-cli dies** → `signal` task detects socket close, restarts the
  child with exponential backoff; incoming messages resume. Dashboard
  shows a "Signal: reconnecting" status.
- **Ollama unreachable / times out** → reply for that message is skipped
  and logged; no crash, no partial send. No "brain offline" chat notice
  by default (avoids noise).
- **Malformed personality TOML** → logged and skipped; bot runs with the
  valid ones. `default` failing to load is fatal at startup.
- **DB errors** → surfaced per-operation. WAL mode + single writer (the
  router) avoids lock contention with the web reader.
- **Every task supervised** — a panic in one task is caught and the task
  restarted; the process stays up.
- Structured logging via `tracing`.

## 8. Security

- Only inbound listener is the axum HTTPS port on the LAN interface,
  login-gated. signal-cli socket and Ollama are localhost/UNIX-socket
  only.
- **Login**: single admin, **argon2**-hashed password, cookie session
  (HttpOnly, Secure, SameSite=Strict). Rate-limited login attempts.
- **TLS**: rustls with a locally-trusted cert. Setup generates a local CA
  + `bot.local` cert (mkcert-style) so viewing devices trust it; falls
  back to self-signed if preferred.
- signal-cli account data (keys) under a restricted dir (`0700`), owned by
  the service user.
- systemd unit runs as a **dedicated non-root user** with `ProtectSystem`,
  `NoNewPrivileges`, `PrivateTmp`.
- Secrets (admin hash, tokens) in a `0600` config file or systemd
  `EnvironmentFile`, never in the repo.

## 9. Debug / test mode

Trait boundaries `SignalTransport` and `LlmBackend` provide the seam that
both tests and debug mode use.

- **`--debug` / `SIGNAL_BOT_DEBUG=1`**: verbose tracing — full prompts
  sent to Ollama, raw LLM responses, relevance-gate verdicts, and every
  routing decision (e.g., "room X, mode proactive, confidence 0.42 < 0.7
  → staying quiet").
- **`--dry-run`**: run the full pipeline but do **not** send to Signal;
  log the reply the bot *would* have posted. For tuning personalities and
  proactive thresholds safely against a live room.
- **`--repl`**: pipe fake messages into the router from the terminal
  (choose room, sender, text) with signal-cli stubbed out — exercise
  routing → LLM → reply without Signal at all.
- **Fixture tests**: mock `SignalTransport` and `LlmBackend` + in-memory
  SQLite drive the router deterministically in CI (no network, Ollama, or
  Signal).

## 10. Setup / registration flow

A documented, mostly-scripted sequence run once on bot.local:

1. Install deps: `signal-cli` + JRE, `ollama`; `ollama pull qwen3:8b`.
2. Create the service user + data dirs (restricted perms).
3. **Register Signal** interactively (manual, admin-driven):
   `signal-cli -a +14433996053 register` (with captcha if prompted) →
   receive the SMS/voice code on Google Voice →
   `signal-cli -a +14433996053 verify <code>`.
4. Generate TLS cert + set admin password (setup script).
5. Drop in personality TOMLs (a `default` + example personalities).
6. Enable the systemd unit.

### Prerequisites / risks

- The Google Voice number must not already be registered to Signal on
  another device; registration requires receiving the verification code.
- `qwen3:8b` needs ~6–10 GB RAM/VRAM to run comfortably. Verify host
  specs during setup; CPU-only works but is slower.

## 11. Technology stack

- **Language/runtime:** Rust, `tokio` async.
- **Web:** `axum` + `rustls` (TLS), SSE for live updates.
- **DB:** SQLite via `sqlx` (WAL, compile-time-checked queries).
- **Config:** `toml` / `serde`.
- **Auth:** `argon2` password hashing.
- **Logging:** `tracing` + `tracing-subscriber`.
- **Signal transport:** `signal-cli` (`--jsonrpc` daemon over UNIX socket).
- **LLM:** Ollama HTTP API serving `qwen3:8b`, localhost-bound.
- **Deploy:** systemd unit, dedicated non-root user.

## 12. Locked defaults (previously open)

These were decided so the implementation plan has no ambiguity:

- **Rolling-window budget:** token-budget based. Fill the window up to
  ~75% of the personality's `num_ctx` (reserving the remainder for the
  system prompt + generation), newest messages first, with a hard floor
  of the last 8 messages and a ceiling of the last 60. Token counting is
  approximate (chars/4 heuristic) in v1.
- **Hot-reload:** watch `personalities/` with the `notify` crate for live
  reload, plus SIGHUP as an explicit fallback trigger.
- **Frontend:** server-rendered HTML templates (`askama`) + minimal
  vanilla JS, with SSE for live message updates. No SPA framework in v1.
