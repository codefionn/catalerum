-- Per-turn token + cost accounting on messages (SOUL §7/§12): persist the agent
-- loop's summed usage for an exchange so a replayed transcript shows the same
-- token info-icon (and cost readout) under the assistant bubble as the live turn
-- did, instead of dropping it on reload (the counts only ever lived on the
-- terminal `message_done` frame).
--
-- All columns sit on the **final assistant message** of an exchange (role =
-- 'assistant'); every other row (user/tool/system, and non-final assistant turns)
-- leaves them NULL. NULL is also the legacy default, so transcripts recorded
-- before this simply show no icon (exactly the prior behaviour). Token counts are
-- BIGINT (matching `tool_duration_ms`) and always non-negative; `cost_usd` is the
-- USD estimate, omitted (NULL) when the model has no known price.
ALTER TABLE messages
    ADD COLUMN prompt_tokens         BIGINT,
    ADD COLUMN completion_tokens     BIGINT,
    ADD COLUMN total_tokens          BIGINT,
    ADD COLUMN cached_tokens         BIGINT,
    ADD COLUMN cache_creation_tokens BIGINT,
    ADD COLUMN cost_usd              DOUBLE PRECISION;
