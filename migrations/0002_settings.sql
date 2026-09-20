CREATE TABLE settings (
    id                     INTEGER PRIMARY KEY CHECK (id = 1),
    keep_alive             TEXT NOT NULL,
    ollama_timeout_secs    INTEGER NOT NULL,
    repeat_penalty         REAL NOT NULL,
    repeat_last_n          INTEGER NOT NULL,
    num_predict            INTEGER NOT NULL,
    num_ctx                INTEGER NOT NULL,
    default_temperature    REAL NOT NULL,
    default_top_p          REAL NOT NULL,
    summary_enabled        INTEGER NOT NULL,
    summary_interval_hours INTEGER NOT NULL
);
