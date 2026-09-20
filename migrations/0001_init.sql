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
