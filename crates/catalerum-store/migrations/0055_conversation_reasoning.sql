-- catalerum-store — bind a per-conversation reasoning ("thinking") effort (SOUL §7/§12).
--
-- The chat "thinking" picker (Settings tab): a conversation may request that its
-- §7 loop think with a given reasoning effort, independent of the model. It is
-- carried onto the chat `ChatRequest.reasoning_effort` for the turn; NULL = no
-- reasoning requested (the provider default).
--
-- A free-form TEXT effort token (`low` | `medium` | `high` | `xhigh` | `max`) — the
-- gateway passes it through to the model — mirroring `model` above it: not an FK
-- to any catalog, since the provider's accepted set is external and dynamic. The
-- conversation row is already workspace-scoped, and the value is just a string.

ALTER TABLE conversations
    ADD COLUMN reasoning_effort TEXT;
