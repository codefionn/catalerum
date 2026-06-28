-- Per-user time compression for microphone audio before speech-to-text.
-- 1.5 shortens a recorded take to two thirds of its original duration.
ALTER TABLE llm_settings
    ADD COLUMN voice_input_speed REAL NOT NULL DEFAULT 1.5
        CHECK (voice_input_speed >= 1.0 AND voice_input_speed <= 2.0);
