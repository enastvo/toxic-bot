-- Per-personality extra web-search domains, editable from the dashboard.
-- Merged (deduped) with the personality TOML's `extra_search_domains` and the
-- global whitelist at reply time. Keyed by personality name; value is the same
-- comma/space/newline-separated domain string format as settings.search_whitelist.
CREATE TABLE persona_search_domains (
    persona  TEXT PRIMARY KEY,
    domains  TEXT NOT NULL DEFAULT ''
);
