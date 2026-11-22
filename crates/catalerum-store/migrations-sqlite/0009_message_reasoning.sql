-- SQLite mirror of Postgres migration 0073 (`messages.reasoning`, SOUL §7/§12).
-- See migrations/0073_message_reasoning.sql for the rationale. JSON is TEXT here.

ALTER TABLE messages
    ADD COLUMN reasoning TEXT;
