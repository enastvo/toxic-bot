-- Operator context reset: messages with ts <= this are excluded from the room's
-- reply context window (used by !reset to break a style lock-in without deleting
-- history). NULL = no cutoff.
ALTER TABLE rooms ADD COLUMN context_cutoff_ts INTEGER;
