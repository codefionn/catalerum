-- catalerum-store — per-user "force image input" model override (SOUL §7/§9).
--
-- Some gateway-catalog models under-report their `input_modalities`, so the chat
-- image-inlining gate (a vision model SEES an uploaded image, not just its text
-- reference) would refuse a model that actually accepts images. This column is the
-- per-user escape hatch: a JSON array of model ids to treat as image-capable
-- regardless of what the catalog advertises. The union of this list, the global
-- `[llm].image_input_models` config list, and the catalog decides whether a turn
-- inlines an image. Empty by default; mirrors how the other model/voice columns
-- are a per-user override of the `[llm]` config defaults.

ALTER TABLE llm_settings
    ADD COLUMN image_input_models JSONB NOT NULL DEFAULT '[]'::jsonb;
