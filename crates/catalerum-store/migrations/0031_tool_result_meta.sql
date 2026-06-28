-- Tool-result metadata on messages (SOUL §12): persist whether a `tool` result
-- row was an error and how long its dispatch took, so a replayed transcript shows
-- the same success/error state and timing as the live tool cards (instead of
-- inferring the error from a `{"error":…}` content shape and dropping timing).
--
-- Both columns sit on the `tool` result row (role = 'tool'); they default for
-- legacy rows (is_error = false → replay falls back to the content heuristic;
-- duration NULL → no timing shown for old turns).
ALTER TABLE messages
    ADD COLUMN tool_is_error    BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN tool_duration_ms BIGINT;
