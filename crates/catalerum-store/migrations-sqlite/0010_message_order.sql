-- SQLite only: keep `messages` in insertion order within a conversation.
--
-- Replay sorts by `(created_at, id)`. `CURRENT_TIMESTAMP` has one-second
-- resolution here and the id is a random UUID, so the rows of a fast tool round
-- (the assistant's call and its results, all inside one second) came back in a
-- random order. A tool result ahead of its call makes the model API reject the
-- next request, which ended the chat. Postgres stamps microseconds and has no
-- such tie.
--
-- The trigger restamps each new row with millisecond precision, and never
-- earlier than 2 ms after the conversation's newest row, so two rows of one
-- conversation cannot tie. Rows written before this migration keep their
-- timestamp; `2026-01-01 10:00:00` sorts before `2026-01-01 10:00:00.002`.

CREATE TRIGGER messages_created_at_order
AFTER INSERT ON messages
FOR EACH ROW
BEGIN
    UPDATE messages
    SET created_at = strftime(
        '%Y-%m-%d %H:%M:%f',
        max(
            julianday('now'),
            coalesce(
                julianday((
                    SELECT max(m.created_at)
                    FROM messages m
                    WHERE m.conversation_id = NEW.conversation_id
                      AND m.id <> NEW.id
                )) + 0.002 / 86400.0,
                0
            )
        )
    )
    WHERE id = NEW.id;
END;
