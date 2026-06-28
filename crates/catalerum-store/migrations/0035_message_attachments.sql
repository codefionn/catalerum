-- File/image references attached to a chat message (SOUL §9/§12).
--
-- A chat upload does NOT embed bytes in the message: the bytes go to a storage
-- backend (the user's default files store) and the message keeps only a
-- reference — the same `Attachment` shape calendar events use (a fetchable
-- `/storage/objects/{key}` url plus display metadata). The agent loop renders
-- these references into the turn so the model can `stage_object`/`copy_object`/
-- `read_object` them, instead of inlining the blob.
--
-- JSONB array, defaulting to `[]` so every existing row reads back as
-- "no attachments" (the prior behaviour). NOT NULL keeps the read path branch-free.
ALTER TABLE messages
    ADD COLUMN attachments JSONB NOT NULL DEFAULT '[]'::jsonb;
