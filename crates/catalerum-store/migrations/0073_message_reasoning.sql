-- catalerum-store — `messages.reasoning` (SOUL §7/§12).
--
-- An assistant turn's reasoning blocks, with the model that produced them:
-- `{"model": "<model id>", "details": [<reasoning detail>, …]}`. A reasoning
-- model signs or encrypts these blocks and expects them back unchanged, so a
-- conversation resumed from the store has to replay what the live turn saw.
--
-- Additive: NULL on every existing row and on rows without reasoning.

ALTER TABLE messages
    ADD COLUMN reasoning JSONB;
