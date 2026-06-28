-- catalerum-store — M8 email read-ingest schema (SOUL §6.1, §28, §10).
--
-- Mailboxes + emails, the provider-agnostic analogue of calendars + events
-- (§8): a mailbox belongs to an email-kind connection; an email belongs to a
-- mailbox. Ingest is idempotent + incremental (principle 4): emails upsert by
-- (mailbox_id, uid) — `uid` is the provider's stable id (IMAP UID, JMAP id,
-- Maildir base filename) — so re-running a sync never duplicates (§3.4). Every
-- tenant row carries `workspace_id` (§18); all repo queries are workspace-filtered.
-- catalerum READS mail; it is not a mail client (no send/reply, §14).

-- ---------------------------------------------------------------------------
-- mailboxes — a folder exposed by an email connection. `external_id` is the
-- provider-native identifier; unique per connection so re-listing upserts
-- (mirrors `calendars`).
-- ---------------------------------------------------------------------------
CREATE TABLE mailboxes (
    id             UUID PRIMARY KEY,
    workspace_id   UUID        NOT NULL REFERENCES workspaces (id)  ON DELETE CASCADE,
    connection_id  UUID        NOT NULL REFERENCES connections (id) ON DELETE CASCADE,
    external_id    TEXT        NOT NULL,
    name           TEXT        NOT NULL,
    read_only      BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT mailboxes_connection_external_uq UNIQUE (connection_id, external_id)
);

CREATE INDEX mailboxes_workspace_idx  ON mailboxes (workspace_id);
CREATE INDEX mailboxes_connection_idx ON mailboxes (connection_id);

-- ---------------------------------------------------------------------------
-- emails — a normalized message. `From`/`To`/`Cc` keep the raw addresses as
-- JSONB (the provider's truth; Person/EntityRef resolution is a derived graph
-- step, §6.3); `flags` is a JSONB array of provider-native flag tokens. Bodies
-- are stored for chunk/embed (§10). The UNIQUE (mailbox_id, uid) makes
-- incremental sync idempotent: emails upsert by uid (INSERT … ON CONFLICT DO
-- UPDATE), so re-running never duplicates (§3.4). `message_id` is indexed for
-- cross-folder dedup (§29).
-- ---------------------------------------------------------------------------
CREATE TABLE emails (
    id               UUID PRIMARY KEY,
    workspace_id     UUID        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    mailbox_id       UUID        NOT NULL REFERENCES mailboxes (id)  ON DELETE CASCADE,
    uid              TEXT        NOT NULL,
    message_id       TEXT,
    from_addr        JSONB       NOT NULL DEFAULT 'null'::jsonb,
    to_addrs         JSONB       NOT NULL DEFAULT '[]'::jsonb,
    cc_addrs         JSONB       NOT NULL DEFAULT '[]'::jsonb,
    subject          TEXT        NOT NULL DEFAULT '',
    received_at      TIMESTAMPTZ,
    body_text        TEXT,
    body_html        TEXT,
    has_attachments  BOOLEAN     NOT NULL DEFAULT FALSE,
    flags            JSONB       NOT NULL DEFAULT '[]'::jsonb,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT emails_mailbox_uid_uq UNIQUE (mailbox_id, uid)
);

-- "Recent mail in a workspace" + per-mailbox listing + cross-folder dedup.
CREATE INDEX emails_workspace_received_idx ON emails (workspace_id, received_at DESC);
CREATE INDEX emails_mailbox_idx            ON emails (mailbox_id);
CREATE INDEX emails_message_id_idx         ON emails (workspace_id, message_id);
