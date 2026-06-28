-- catalerum-store — event labels + attachments (SOUL §8/§9).
--
-- Two new JSONB columns on `events`, mirroring how `attendees` is stored:
--   * `labels`      — a JSON array of category strings (iCalendar `CATEGORIES`).
--                     Projected to the derived graph as `:Topic` nodes (§6.3),
--                     like note tags, so "what's on my calendar near topic X" works.
--   * `attachments` — a JSON array of attachment descriptors (iCalendar `ATTACH`):
--                     `{ "url", "filename"?, "content_type"?, "size"? }`. An
--                     uploaded file (a workspace storage path) or an external link.
--
-- Both default to an empty array, so every existing event is unchanged and the
-- upsert/patch paths never need a backfill. NOT NULL keeps the `Json<Vec<…>>`
-- row decode total (no `Option` wrapper needed).

ALTER TABLE events
    ADD COLUMN labels      JSONB NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN attachments JSONB NOT NULL DEFAULT '[]'::jsonb;
