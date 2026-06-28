PRAGMA foreign_keys = ON;
CREATE TABLE agent_profiles (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  name TEXT NOT NULL,
  model TEXT,
  system_prompt TEXT,
  tools TEXT NOT NULL DEFAULT '[]',
  skills TEXT NOT NULL DEFAULT '[]',
  subagents TEXT NOT NULL DEFAULT '[]',
  channels TEXT NOT NULL DEFAULT '[]',
  grant_id BLOB,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  guard TEXT,
  CONSTRAINT agent_profiles_grant_id_fk FOREIGN KEY (workspace_id, grant_id) REFERENCES grants(workspace_id, id) ON DELETE SET NULL,
  CONSTRAINT agent_profiles_pkey PRIMARY KEY (id),
  CONSTRAINT agent_profiles_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
  CONSTRAINT agent_profiles_workspace_id_id_key UNIQUE (workspace_id, id),
  CONSTRAINT agent_profiles_workspace_id_name_key UNIQUE (workspace_id, name)
);

CREATE TABLE app_data (
  workspace_id BLOB NOT NULL,
  app TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT app_data_pkey PRIMARY KEY (workspace_id, app, key),
  CONSTRAINT app_data_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE automation_runs (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  automation_id BLOB NOT NULL,
  status TEXT NOT NULL,
  trigger TEXT,
  error TEXT,
  started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  finished_at TEXT,
  job_id BLOB,
  grant_id BLOB,
  CONSTRAINT automation_runs_automation_id_fkey FOREIGN KEY (automation_id) REFERENCES automations(id) ON DELETE CASCADE,
  CONSTRAINT automation_runs_pkey PRIMARY KEY (id),
  CONSTRAINT automation_runs_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE automation_steps (
  id BLOB NOT NULL,
  run_id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  ordinal INTEGER NOT NULL,
  action TEXT NOT NULL,
  status TEXT NOT NULL,
  output TEXT,
  error TEXT,
  started_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  finished_at TEXT,
  CONSTRAINT automation_steps_pkey PRIMARY KEY (id),
  CONSTRAINT automation_steps_run_id_fkey FOREIGN KEY (run_id) REFERENCES automation_runs(id) ON DELETE CASCADE,
  CONSTRAINT automation_steps_run_id_ordinal_key UNIQUE (run_id, ordinal),
  CONSTRAINT automation_steps_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE automations (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  triggers TEXT NOT NULL DEFAULT '[]',
  condition TEXT,
  actions TEXT NOT NULL DEFAULT '[]',
  spec TEXT,
  grant_id BLOB,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT automations_grant_id_fk FOREIGN KEY (workspace_id, grant_id) REFERENCES grants(workspace_id, id) ON DELETE SET NULL,
  CONSTRAINT automations_pkey PRIMARY KEY (id),
  CONSTRAINT automations_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
  CONSTRAINT automations_workspace_id_name_key UNIQUE (workspace_id, name)
);

CREATE TABLE board_columns (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  board_id BLOB NOT NULL,
  name TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT board_columns_board_id_fkey FOREIGN KEY (board_id) REFERENCES boards(id) ON DELETE CASCADE,
  CONSTRAINT board_columns_pkey PRIMARY KEY (id),
  CONSTRAINT board_columns_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE boards (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  name TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT boards_pkey PRIMARY KEY (id),
  CONSTRAINT boards_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE buckets (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  connection_id BLOB NOT NULL,
  name TEXT NOT NULL,
  prefix TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT buckets_connection_id_fkey FOREIGN KEY (connection_id) REFERENCES connections(id) ON DELETE CASCADE,
  CONSTRAINT buckets_connection_name_uq UNIQUE (connection_id, name),
  CONSTRAINT buckets_pkey PRIMARY KEY (id),
  CONSTRAINT buckets_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE calendar_exclusions (
  workspace_id BLOB NOT NULL,
  connection_id BLOB NOT NULL,
  external_id TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT calendar_exclusions_connection_id_fkey FOREIGN KEY (connection_id) REFERENCES connections(id) ON DELETE CASCADE,
  CONSTRAINT calendar_exclusions_pkey PRIMARY KEY (connection_id, external_id),
  CONSTRAINT calendar_exclusions_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE calendars (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  connection_id BLOB,
  external_id TEXT NOT NULL,
  name TEXT NOT NULL,
  read_only INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT calendars_connection_external_uq UNIQUE (connection_id, external_id),
  CONSTRAINT calendars_connection_id_fkey FOREIGN KEY (connection_id) REFERENCES connections(id) ON DELETE CASCADE,
  CONSTRAINT calendars_pkey PRIMARY KEY (id),
  CONSTRAINT calendars_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE chunks (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  document_id BLOB NOT NULL,
  ordinal INTEGER NOT NULL,
  text TEXT NOT NULL,
  point_id BLOB,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT chunks_document_id_fkey FOREIGN KEY (document_id) REFERENCES documents(id) ON DELETE CASCADE,
  CONSTRAINT chunks_document_id_ordinal_key UNIQUE (document_id, ordinal),
  CONSTRAINT chunks_pkey PRIMARY KEY (id),
  CONSTRAINT chunks_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE computer_agents (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  user_id BLOB NOT NULL,
  name TEXT NOT NULL,
  token_hash TEXT NOT NULL,
  platform TEXT,
  capabilities TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  last_seen_at TEXT,
  revoked_at TEXT,
  CONSTRAINT computer_agents_pkey PRIMARY KEY (id),
  CONSTRAINT computer_agents_token_hash_key UNIQUE (token_hash),
  CONSTRAINT computer_agents_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
  CONSTRAINT computer_agents_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
  CONSTRAINT computer_agents_workspace_id_name_key UNIQUE (workspace_id, name)
);

CREATE TABLE connections (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  credential_ref TEXT,
  config TEXT NOT NULL DEFAULT '{}',
  sync_token TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT connections_pkey PRIMARY KEY (id),
  CONSTRAINT connections_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
  CONSTRAINT connections_workspace_kind_name_uq UNIQUE (workspace_id, kind, name)
);

CREATE TABLE conversations (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  title TEXT,
  origin TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  agent_profile_id BLOB,
  model TEXT,
  summary TEXT,
  summary_upto BLOB,
  reasoning_effort TEXT,
  CONSTRAINT conversations_agent_profile_id_fk FOREIGN KEY (workspace_id, agent_profile_id) REFERENCES agent_profiles(workspace_id, id) ON DELETE SET NULL,
  CONSTRAINT conversations_pkey PRIMARY KEY (id),
  CONSTRAINT conversations_summary_upto_fkey FOREIGN KEY (summary_upto) REFERENCES messages(id) ON DELETE SET NULL,
  CONSTRAINT conversations_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE documents (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  source_kind TEXT NOT NULL,
  source_id TEXT NOT NULL,
  text TEXT NOT NULL DEFAULT '',
  summary TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT documents_pkey PRIMARY KEY (id),
  CONSTRAINT documents_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
  CONSTRAINT documents_workspace_id_source_kind_source_id_key UNIQUE (workspace_id, source_kind, source_id)
);

CREATE TABLE emails (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  mailbox_id BLOB NOT NULL,
  uid TEXT NOT NULL,
  message_id TEXT,
  from_addr TEXT NOT NULL DEFAULT NULL,
  to_addrs TEXT NOT NULL DEFAULT '[]',
  cc_addrs TEXT NOT NULL DEFAULT '[]',
  subject TEXT NOT NULL DEFAULT '',
  received_at TEXT,
  body_text TEXT,
  body_html TEXT,
  has_attachments INTEGER NOT NULL DEFAULT 0,
  flags TEXT NOT NULL DEFAULT '[]',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  raw_ref TEXT,
  labels TEXT NOT NULL DEFAULT '[]',
  attachments TEXT NOT NULL DEFAULT '[]',
  CONSTRAINT emails_mailbox_id_fkey FOREIGN KEY (mailbox_id) REFERENCES mailboxes(id) ON DELETE CASCADE,
  CONSTRAINT emails_mailbox_uid_uq UNIQUE (mailbox_id, uid),
  CONSTRAINT emails_pkey PRIMARY KEY (id),
  CONSTRAINT emails_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE events (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  calendar_id BLOB NOT NULL,
  uid TEXT NOT NULL,
  starts_at TEXT NOT NULL,
  ends_at TEXT NOT NULL,
  all_day INTEGER NOT NULL DEFAULT 0,
  rrule TEXT,
  summary TEXT NOT NULL,
  location TEXT,
  body TEXT,
  attendees TEXT NOT NULL DEFAULT '[]',
  etag TEXT,
  sequence INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  labels TEXT NOT NULL DEFAULT '[]',
  attachments TEXT NOT NULL DEFAULT '[]',
  CONSTRAINT events_calendar_id_fkey FOREIGN KEY (calendar_id) REFERENCES calendars(id) ON DELETE CASCADE,
  CONSTRAINT events_calendar_uid_uq UNIQUE (calendar_id, uid),
  CONSTRAINT events_pkey PRIMARY KEY (id),
  CONSTRAINT events_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE external_db_migration_scripts (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  connection_id BLOB NOT NULL,
  version INTEGER NOT NULL,
  name TEXT NOT NULL,
  up_sql TEXT NOT NULL,
  checksum TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT external_db_migration_scripts_connection_id_fkey FOREIGN KEY (connection_id) REFERENCES connections(id) ON DELETE CASCADE,
  CONSTRAINT external_db_migration_scripts_pkey PRIMARY KEY (id),
  CONSTRAINT external_db_migration_scripts_version_uq UNIQUE (connection_id, version),
  CONSTRAINT external_db_migration_scripts_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE external_db_migrations (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  connection_id BLOB NOT NULL,
  version INTEGER NOT NULL,
  name TEXT NOT NULL,
  checksum TEXT NOT NULL,
  applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT external_db_migrations_connection_id_fkey FOREIGN KEY (connection_id) REFERENCES connections(id) ON DELETE CASCADE,
  CONSTRAINT external_db_migrations_pkey PRIMARY KEY (id),
  CONSTRAINT external_db_migrations_version_uq UNIQUE (connection_id, version),
  CONSTRAINT external_db_migrations_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE grants (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  name TEXT NOT NULL,
  capabilities TEXT NOT NULL DEFAULT '[]',
  constraints TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT grants_pkey PRIMARY KEY (id),
  CONSTRAINT grants_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
  CONSTRAINT grants_workspace_id_id_key UNIQUE (workspace_id, id),
  CONSTRAINT grants_workspace_id_name_key UNIQUE (workspace_id, name)
);

CREATE TABLE job_queue (
  id BLOB NOT NULL,
  workspace_id BLOB,
  kind TEXT NOT NULL,
  payload TEXT NOT NULL DEFAULT '{}',
  status TEXT NOT NULL DEFAULT 'pending',
  attempts INTEGER NOT NULL DEFAULT 0,
  run_after TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  locked_at TEXT,
  locked_by TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT job_queue_pkey PRIMARY KEY (id),
  CONSTRAINT job_queue_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE links (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  from_kind TEXT NOT NULL,
  from_id TEXT NOT NULL,
  to_kind TEXT NOT NULL,
  to_id TEXT NOT NULL,
  label TEXT,
  note TEXT,
  author_kind TEXT NOT NULL,
  author_id BLOB NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT links_pkey PRIMARY KEY (id),
  CONSTRAINT links_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE llm_settings (
  workspace_id BLOB NOT NULL,
  user_id BLOB NOT NULL,
  chat_model TEXT,
  speech_model TEXT,
  speech_voice TEXT,
  transcription_model TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  image_input_models TEXT NOT NULL DEFAULT '[]',
  ocr_model TEXT,
  voice_input_speed REAL NOT NULL DEFAULT 1.5,
  CONSTRAINT llm_settings_pkey PRIMARY KEY (workspace_id, user_id),
  CONSTRAINT llm_settings_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE login_tokens (
  token_hash TEXT NOT NULL,
  user_id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  expires_at TEXT NOT NULL,
  consumed_at TEXT,
  CONSTRAINT login_tokens_pkey PRIMARY KEY (token_hash),
  CONSTRAINT login_tokens_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
  CONSTRAINT login_tokens_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE mailboxes (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  connection_id BLOB NOT NULL,
  external_id TEXT NOT NULL,
  name TEXT NOT NULL,
  read_only INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT mailboxes_connection_external_uq UNIQUE (connection_id, external_id),
  CONSTRAINT mailboxes_connection_id_fkey FOREIGN KEY (connection_id) REFERENCES connections(id) ON DELETE CASCADE,
  CONSTRAINT mailboxes_pkey PRIMARY KEY (id),
  CONSTRAINT mailboxes_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE mcp_endpoints (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  name TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  script TEXT NOT NULL DEFAULT '',
  bucket_name TEXT,
  key_prefix TEXT,
  grant_id BLOB,
  enabled INTEGER NOT NULL DEFAULT 1,
  author_kind TEXT NOT NULL,
  author_id BLOB NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT mcp_endpoints_grant_id_fkey FOREIGN KEY (grant_id) REFERENCES grants(id) ON DELETE SET NULL,
  CONSTRAINT mcp_endpoints_pkey PRIMARY KEY (id),
  CONSTRAINT mcp_endpoints_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
  CONSTRAINT mcp_endpoints_workspace_id_name_key UNIQUE (workspace_id, name)
);

CREATE TABLE mcp_servers (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  name TEXT NOT NULL,
  transport TEXT NOT NULL DEFAULT 'stdio',
  command TEXT NOT NULL DEFAULT '',
  args TEXT NOT NULL DEFAULT '[]',
  env TEXT NOT NULL DEFAULT '{}',
  url TEXT NOT NULL DEFAULT '',
  auth TEXT NOT NULL DEFAULT '{}',
  enabled INTEGER NOT NULL DEFAULT 1,
  tools TEXT NOT NULL DEFAULT '[]',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT mcp_servers_pkey PRIMARY KEY (id),
  CONSTRAINT mcp_servers_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
  CONSTRAINT mcp_servers_workspace_id_name_key UNIQUE (workspace_id, name)
);

CREATE TABLE memberships (
  workspace_id BLOB NOT NULL,
  user_id BLOB NOT NULL,
  role TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT memberships_pkey PRIMARY KEY (workspace_id, user_id),
  CONSTRAINT memberships_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
  CONSTRAINT memberships_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE memories (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  scope TEXT NOT NULL,
  user_id BLOB,
  text TEXT NOT NULL,
  source_kind TEXT,
  source_id TEXT,
  point_id BLOB,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT memories_pkey PRIMARY KEY (id),
  CONSTRAINT memories_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE messages (
  id BLOB NOT NULL,
  conversation_id BLOB NOT NULL,
  role TEXT NOT NULL,
  content TEXT NOT NULL,
  tool_calls TEXT NOT NULL DEFAULT '[]',
  tool_call_id TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  tool_is_error INTEGER NOT NULL DEFAULT 0,
  tool_duration_ms INTEGER,
  prompt_tokens INTEGER,
  completion_tokens INTEGER,
  total_tokens INTEGER,
  cached_tokens INTEGER,
  cache_creation_tokens INTEGER,
  cost_usd REAL,
  attachments TEXT NOT NULL DEFAULT '[]',
  skill TEXT,
  CONSTRAINT messages_conversation_id_fkey FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
  CONSTRAINT messages_pkey PRIMARY KEY (id)
);

CREATE TABLE notes (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  author_kind TEXT NOT NULL,
  author_id BLOB NOT NULL,
  title TEXT NOT NULL,
  markdown TEXT NOT NULL DEFAULT '',
  tags TEXT NOT NULL DEFAULT '[]',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT notes_pkey PRIMARY KEY (id),
  CONSTRAINT notes_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE object_labels (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  store TEXT NOT NULL DEFAULT '',
  path TEXT NOT NULL,
  is_dir INTEGER NOT NULL DEFAULT 0,
  label TEXT NOT NULL,
  author_kind TEXT NOT NULL,
  author_id BLOB NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT object_labels_pkey PRIMARY KEY (id),
  CONSTRAINT object_labels_uniq UNIQUE (workspace_id, store, path, label),
  CONSTRAINT object_labels_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE objects (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  bucket_id BLOB NOT NULL,
  key TEXT NOT NULL,
  size INTEGER NOT NULL DEFAULT 0,
  content_type TEXT,
  etag TEXT,
  last_modified TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  sha256 TEXT,
  extracted_text_id BLOB,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT objects_bucket_id_fkey FOREIGN KEY (bucket_id) REFERENCES buckets(id) ON DELETE CASCADE,
  CONSTRAINT objects_bucket_key_uq UNIQUE (bucket_id, key),
  CONSTRAINT objects_extracted_text_id_fkey FOREIGN KEY (extracted_text_id) REFERENCES documents(id) ON DELETE SET NULL,
  CONSTRAINT objects_pkey PRIMARY KEY (id),
  CONSTRAINT objects_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE org_memberships (
  organisation_id BLOB NOT NULL,
  user_id BLOB NOT NULL,
  role TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT org_memberships_organisation_id_fkey FOREIGN KEY (organisation_id) REFERENCES organisations(id) ON DELETE CASCADE,
  CONSTRAINT org_memberships_pkey PRIMARY KEY (organisation_id, user_id),
  CONSTRAINT org_memberships_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);

CREATE TABLE organisations (
  id BLOB NOT NULL,
  name TEXT NOT NULL,
  slug TEXT NOT NULL,
  workspace_creation TEXT NOT NULL DEFAULT 'members',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT organisations_pkey PRIMARY KEY (id),
  CONSTRAINT organisations_slug_key UNIQUE (slug)
);

CREATE TABLE pending_approvals (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  conversation_id BLOB NOT NULL,
  tool TEXT NOT NULL,
  arguments TEXT NOT NULL,
  reason TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  resolved_at TEXT,
  decision TEXT,
  CONSTRAINT pending_approvals_conversation_id_fkey FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
  CONSTRAINT pending_approvals_pkey PRIMARY KEY (id),
  CONSTRAINT pending_approvals_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE pending_questions (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  conversation_id BLOB NOT NULL,
  questions TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  resolved_at TEXT,
  answers TEXT,
  CONSTRAINT pending_questions_conversation_id_fkey FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE,
  CONSTRAINT pending_questions_pkey PRIMARY KEY (id),
  CONSTRAINT pending_questions_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE pod_heartbeats (
  pod_id TEXT NOT NULL,
  last_seen TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT pod_heartbeats_pkey PRIMARY KEY (pod_id)
);

CREATE TABLE profiles (
  workspace_id BLOB NOT NULL,
  user_id BLOB NOT NULL,
  fields TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT profiles_pkey PRIMARY KEY (workspace_id, user_id),
  CONSTRAINT profiles_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE search_settings (
  workspace_id BLOB NOT NULL,
  user_id BLOB NOT NULL,
  default_provider TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT search_settings_pkey PRIMARY KEY (workspace_id, user_id),
  CONSTRAINT search_settings_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE secret_store (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  ref TEXT NOT NULL,
  nonce BLOB NOT NULL,
  ciphertext BLOB NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT secret_store_pkey PRIMARY KEY (id),
  CONSTRAINT secret_store_ref_uq UNIQUE (workspace_id, ref),
  CONSTRAINT secret_store_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE sessions (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  user_id BLOB NOT NULL,
  token_hash TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  expires_at TEXT NOT NULL,
  grant_id BLOB,
  CONSTRAINT sessions_grant_id_fk FOREIGN KEY (workspace_id, grant_id) REFERENCES grants(workspace_id, id) ON DELETE CASCADE,
  CONSTRAINT sessions_pkey PRIMARY KEY (id),
  CONSTRAINT sessions_token_hash_key UNIQUE (token_hash),
  CONSTRAINT sessions_user_id_fkey FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
  CONSTRAINT sessions_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE skills (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  name TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  instructions_md TEXT NOT NULL DEFAULT '',
  tools TEXT NOT NULL DEFAULT '[]',
  code TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  advertised INTEGER NOT NULL DEFAULT 1,
  CONSTRAINT skills_pkey PRIMARY KEY (id),
  CONSTRAINT skills_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE,
  CONSTRAINT skills_workspace_id_name_key UNIQUE (workspace_id, name)
);

CREATE TABLE storage_settings (
  workspace_id BLOB NOT NULL,
  user_id BLOB NOT NULL,
  default_store TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT storage_settings_pkey PRIMARY KEY (workspace_id, user_id),
  CONSTRAINT storage_settings_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE tasks (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  board_id BLOB NOT NULL,
  column_id BLOB NOT NULL,
  title TEXT NOT NULL,
  body_md TEXT NOT NULL DEFAULT '',
  assignee_kind TEXT,
  assignee_id BLOB,
  ordinal INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'open',
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT tasks_board_id_fkey FOREIGN KEY (board_id) REFERENCES boards(id) ON DELETE CASCADE,
  CONSTRAINT tasks_column_id_fkey FOREIGN KEY (column_id) REFERENCES board_columns(id) ON DELETE CASCADE,
  CONSTRAINT tasks_pkey PRIMARY KEY (id),
  CONSTRAINT tasks_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE terminal_sessions (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  backend TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'active',
  host_dir TEXT,
  sync_prefix TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  closed_at TEXT,
  pod_id TEXT,
  CONSTRAINT terminal_sessions_pkey PRIMARY KEY (id),
  CONSTRAINT terminal_sessions_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE ui_definitions (
  id BLOB NOT NULL,
  workspace_id BLOB NOT NULL,
  author_kind TEXT NOT NULL,
  author_id BLOB NOT NULL,
  name TEXT,
  title TEXT NOT NULL,
  description TEXT,
  spec_version INTEGER NOT NULL DEFAULT 1,
  version INTEGER NOT NULL DEFAULT 1,
  definition TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT ui_definitions_pkey PRIMARY KEY (id),
  CONSTRAINT ui_definitions_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE users (
  id BLOB NOT NULL,
  email TEXT NOT NULL,
  display_name TEXT NOT NULL,
  sso_issuer TEXT,
  sso_subject TEXT,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT users_email_key UNIQUE (email),
  CONSTRAINT users_pkey PRIMARY KEY (id)
);

CREATE TABLE workspace_sandboxes (
  workspace_id BLOB NOT NULL,
  backend TEXT NOT NULL,
  image TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'pending',
  container_ref TEXT,
  volume_ref TEXT,
  last_activity TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  pod_id TEXT,
  CONSTRAINT workspace_sandboxes_pkey PRIMARY KEY (workspace_id),
  CONSTRAINT workspace_sandboxes_workspace_id_fkey FOREIGN KEY (workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE workspaces (
  id BLOB NOT NULL,
  name TEXT NOT NULL,
  slug TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
  organisation_id BLOB NOT NULL,
  archived_at TEXT,
  CONSTRAINT workspaces_organisation_id_fk FOREIGN KEY (organisation_id) REFERENCES organisations(id),
  CONSTRAINT workspaces_pkey PRIMARY KEY (id),
  CONSTRAINT workspaces_slug_key UNIQUE (slug)
);

CREATE INDEX agent_profiles_workspace_idx ON agent_profiles (workspace_id);
CREATE INDEX automation_runs_automation_idx ON automation_runs (automation_id, started_at DESC);
CREATE INDEX automation_runs_job_idx ON automation_runs (job_id) WHERE (job_id IS NOT NULL);
CREATE INDEX board_columns_board_idx ON board_columns (board_id, ordinal);
CREATE INDEX buckets_connection_idx ON buckets (connection_id);
CREATE INDEX buckets_workspace_idx ON buckets (workspace_id);
CREATE INDEX calendar_exclusions_workspace_idx ON calendar_exclusions (workspace_id);
CREATE INDEX calendars_connection_idx ON calendars (connection_id);
CREATE UNIQUE INDEX calendars_local_external_uq ON calendars (workspace_id, external_id) WHERE (connection_id IS NULL);
CREATE INDEX calendars_workspace_idx ON calendars (workspace_id);
CREATE INDEX chunks_document_ordinal_idx ON chunks (document_id, ordinal);
CREATE INDEX chunks_workspace_idx ON chunks (workspace_id);
CREATE INDEX computer_agents_workspace_idx ON computer_agents (workspace_id, created_at DESC);
CREATE INDEX connections_kind_idx ON connections (workspace_id, kind);
CREATE INDEX connections_workspace_idx ON connections (workspace_id, created_at DESC);
CREATE INDEX conversations_workspace_idx ON conversations (workspace_id, created_at DESC);
CREATE INDEX emails_mailbox_idx ON emails (mailbox_id);
CREATE INDEX emails_message_id_idx ON emails (workspace_id, message_id);
CREATE INDEX emails_workspace_received_idx ON emails (workspace_id, received_at DESC);
CREATE INDEX events_calendar_idx ON events (calendar_id);
CREATE INDEX events_workspace_starts_idx ON events (workspace_id, starts_at);
CREATE INDEX external_db_migration_scripts_conn_idx ON external_db_migration_scripts (connection_id, version);
CREATE INDEX external_db_migrations_conn_idx ON external_db_migrations (connection_id, version);
CREATE INDEX grants_workspace_idx ON grants (workspace_id);
CREATE INDEX job_queue_dequeue_idx ON job_queue (status, run_after);
CREATE INDEX job_queue_workspace_idx ON job_queue (workspace_id);
CREATE INDEX links_from_idx ON links (workspace_id, from_kind, from_id);
CREATE INDEX links_to_idx ON links (workspace_id, to_kind, to_id);
CREATE UNIQUE INDEX links_uniq_idx ON links (workspace_id, from_kind, from_id, to_kind, to_id, COALESCE(label, ''));
CREATE INDEX login_tokens_expires_idx ON login_tokens (expires_at);
CREATE INDEX login_tokens_user_idx ON login_tokens (user_id);
CREATE INDEX login_tokens_workspace_idx ON login_tokens (workspace_id);
CREATE INDEX mailboxes_connection_idx ON mailboxes (connection_id);
CREATE INDEX mailboxes_workspace_idx ON mailboxes (workspace_id);
CREATE INDEX mcp_endpoints_ws_updated_idx ON mcp_endpoints (workspace_id, updated_at DESC);
CREATE INDEX mcp_servers_workspace_idx ON mcp_servers (workspace_id);
CREATE INDEX memberships_user_idx ON memberships (user_id);
CREATE INDEX memories_workspace_created_idx ON memories (workspace_id, created_at DESC);
CREATE INDEX memories_workspace_user_idx ON memories (workspace_id, user_id);
CREATE INDEX messages_conversation_idx ON messages (conversation_id, created_at);
CREATE INDEX notes_workspace_updated_idx ON notes (workspace_id, updated_at DESC);
CREATE INDEX object_labels_label_idx ON object_labels (workspace_id, label);
CREATE INDEX object_labels_store_path_idx ON object_labels (workspace_id, store, path);
CREATE INDEX objects_bucket_idx ON objects (bucket_id, key);
CREATE INDEX objects_workspace_idx ON objects (workspace_id, last_modified DESC);
CREATE INDEX org_memberships_user_idx ON org_memberships (user_id);
CREATE INDEX pending_approvals_resolved_idx ON pending_approvals (workspace_id, conversation_id, resolved_at DESC) WHERE (resolved_at IS NOT NULL);
CREATE INDEX pending_approvals_unresolved_idx ON pending_approvals (workspace_id, conversation_id, created_at DESC) WHERE (resolved_at IS NULL);
CREATE INDEX pending_questions_unresolved_idx ON pending_questions (workspace_id, conversation_id, created_at DESC) WHERE (resolved_at IS NULL);
CREATE INDEX secret_store_workspace_idx ON secret_store (workspace_id);
CREATE INDEX sessions_expires_idx ON sessions (expires_at);
CREATE INDEX sessions_grant_id_idx ON sessions (grant_id);
CREATE INDEX sessions_user_idx ON sessions (user_id);
CREATE INDEX sessions_workspace_idx ON sessions (workspace_id);
CREATE INDEX tasks_board_idx ON tasks (board_id);
CREATE INDEX tasks_column_ordinal_idx ON tasks (column_id, ordinal);
CREATE INDEX terminal_sessions_workspace_idx ON terminal_sessions (workspace_id);
CREATE UNIQUE INDEX ui_definitions_ws_name_idx ON ui_definitions (workspace_id, name) WHERE (name IS NOT NULL);
CREATE INDEX ui_definitions_ws_updated_idx ON ui_definitions (workspace_id, updated_at DESC);
CREATE UNIQUE INDEX users_sso_idx ON users (sso_issuer, sso_subject) WHERE (sso_issuer IS NOT NULL);
CREATE INDEX workspace_sandboxes_status_idx ON workspace_sandboxes (status);
CREATE INDEX workspaces_active_organisation_idx ON workspaces (organisation_id) WHERE (archived_at IS NULL);
CREATE INDEX workspaces_organisation_idx ON workspaces (organisation_id);

-- The stable default organisation used by WorkspaceRepo::create. UUID values
-- are stored as native 16-byte blobs by sqlx's SQLite UUID codec.
INSERT INTO organisations (id, name, slug, workspace_creation)
VALUES (X'def00000000040008000000000000000', 'Default', 'default', 'members');
