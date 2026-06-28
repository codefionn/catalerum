-- Conversation auto-title/auto-tag metadata (chat auto titling + tagging).
-- `tags`: free-text topic labels the background generator attaches to a chat
-- thread (rendered as pills in the sidebar); `title_manual`: set by an explicit
-- rename (PUT /conversations/{id}), which pins the title so the background
-- generator must never overwrite a user-chosen name.
ALTER TABLE conversations
    ADD COLUMN tags         JSONB   NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN title_manual BOOLEAN NOT NULL DEFAULT FALSE;
