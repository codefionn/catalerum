-- catalerum-store — email labels (SOUL §11/§28).
--
-- One new JSONB column on `emails`, mirroring how `events.labels` is stored
-- (0024) and how `flags` is stored:
--   * `labels` — a JSON array of free-text category strings applied by automations
--                (a `LabelEmail` action records a classifier verdict, e.g.
--                `["receipt"]`/`["urgent"]`). Distinct from `flags`, which are the
--                provider's own tokens (`seen`/`flagged`); `labels` are
--                catalerum-side categories a user/agent assigns.
--
-- Defaults to an empty array, so every existing email is unchanged and the
-- upsert path needs no backfill. NOT NULL keeps the `Json<Vec<String>>` row decode
-- total (no `Option` wrapper). `LabelEmail` writes it via a dedicated `set_labels`
-- so a flag-only re-sync never clobbers a verdict (same separation as `raw_ref`).

ALTER TABLE emails
    ADD COLUMN labels JSONB NOT NULL DEFAULT '[]'::jsonb;
