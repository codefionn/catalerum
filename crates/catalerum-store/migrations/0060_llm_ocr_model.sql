-- Per-user OCR vision-model override (SOUL §7/§10/§13), mirroring the other
-- nullable model columns: NULL = unset, the configured [ocr] engine chain
-- decides; a value routes user-invoked OCR through the vision engine with it.
ALTER TABLE llm_settings ADD COLUMN ocr_model TEXT;
