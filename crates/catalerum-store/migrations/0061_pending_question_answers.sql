-- catalerum-store — persist the user's answers on resolved `ask_user` question
-- forms (SOUL §7/§12).
--
-- The questions the model asked were already durable (`questions` JSONB), but the
-- structured answers the user picked/typed were flattened into the follow-up user
-- message's prose and then discarded. Store them on the row the answer resolves,
-- so the Q&A exchange survives as data: NULL while the question is pending, and
-- stays NULL when the user typed past the form instead of answering it (a
-- superseded question was never answered).

ALTER TABLE pending_questions ADD COLUMN answers JSONB;
