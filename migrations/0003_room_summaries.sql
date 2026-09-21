CREATE TABLE room_summaries (
    room_id            TEXT PRIMARY KEY REFERENCES rooms(room_id),
    summary            TEXT NOT NULL DEFAULT '',
    covered_through_ts INTEGER NOT NULL DEFAULT 0,
    updated_at         INTEGER NOT NULL
);
