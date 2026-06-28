-- catalerum-store — archive raw email messages in object storage (SOUL §9/§28/§29).
--
-- Email bodies + attachments are heavy, so the **raw RFC 5322 message** (which
-- carries the body AND every attachment as MIME parts) is written to the
-- object-storage backend (S3 / local FS / WebDAV, §9) under a `mail/<email_id>.eml`
-- key, instead of bloating Postgres. `raw_ref` holds that user-facing storage key
-- (workspace-namespaced physically, §18); NULL until the message is archived, or
-- when no storage backend is configured. The extracted `body_text`/`body_html`
-- stay inline as the lightweight projection the inbox listing + §10 search read.
ALTER TABLE emails ADD COLUMN raw_ref TEXT;
