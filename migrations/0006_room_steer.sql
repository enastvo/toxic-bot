-- Per-room operator "steering" directive: a free-text instruction the operator
-- sets from chat (via !steer), injected into that room's system prompt. NULL = none.
ALTER TABLE rooms ADD COLUMN steer TEXT;
