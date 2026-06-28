-- catalerum-store — skill advertising (SOUL §23).
--
-- `advertised` — whether the skill's name + description ride in the chat
-- system prompt ("visible to agent"), so the model reaches for `use_skill`
-- without a discovery round-trip. Default TRUE (advertise; per-skill opt-out).
-- Additive-safe: existing rows keep today's behaviour surface, just made
-- explicit.

ALTER TABLE skills
    ADD COLUMN advertised BOOLEAN NOT NULL DEFAULT TRUE;
