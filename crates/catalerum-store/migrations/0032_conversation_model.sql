-- catalerum-store — bind a per-conversation chat model override (SOUL §7/§12).
--
-- The chat "model" picker (Settings tab): a conversation may pin the model its
-- §7 loop thinks with, independent of the user's workspace default. It is the
-- most specific explicit choice for the thread, so the ws handler lets it win
-- over both a bound agent profile's pinned model and the user/workspace default.
-- NULL = no override (fall back to the bound profile's model, then the default).
--
-- A free-form TEXT model id (the gateway routes it), mirroring the user's
-- `llm_settings.chat_model` — not an FK to any catalog, since the gateway's model
-- set is external and dynamic. Workspace scoping needs no extra constraint: the
-- conversation row is already workspace-scoped, and the value is just a string.

ALTER TABLE conversations
    ADD COLUMN model TEXT;
