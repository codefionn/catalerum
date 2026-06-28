-- Email attachment references (SOUL §9/§28/§29).
--
-- Resolves the §29 open question "email attachments — object bucket + link vs.
-- inline as chunks of the email document" in favour of **bucket + link**: each
-- attachment of a collected message is archived as a separate object in the
-- workspace's files store and referenced here (mirroring how `raw_ref` (0019)
-- links the archived raw `.eml`, and how `labels` (0028) was added additively).
-- The bytes never live in Postgres — this column holds only the reference list
-- (`url` = `/storage/objects/<key>`, plus display metadata). Additive and
-- backfill-safe: existing rows default to an empty array; `WriteEmail`'s upsert
-- leaves it untouched so a flag-only re-sync never clobbers archived refs
-- (`EmailRepo::set_attachments` is the only writer, like `set_raw_ref`).
ALTER TABLE emails
    ADD COLUMN attachments JSONB NOT NULL DEFAULT '[]'::jsonb;
