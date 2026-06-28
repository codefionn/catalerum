-- Conversation auto-title/auto-tag metadata (sqlite mirror of 0069 pg).
ALTER TABLE conversations ADD COLUMN tags TEXT NOT NULL DEFAULT '[]';
ALTER TABLE conversations ADD COLUMN title_manual INTEGER NOT NULL DEFAULT 0;
