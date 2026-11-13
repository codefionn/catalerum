use super::*;
use catalerum_automation::LlmAgent;
use catalerum_core::model::Email;
use super::collect::local_calendar_external_id;
use super::prompt::{firing_event, MAX_SUMMARIZE_INPUT_BYTES};
use crate::tools::{build_registry, NoteIngest};
use catalerum_automation::{execute, FailCodeRunner};
use catalerum_core::model::{Author, Role, RunStatus, StepStatus};
use catalerum_store::{NewAutomation, Store};
use serde_json::json;

fn db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn automation(name: &str, actions: Vec<Value>) -> NewAutomation {
    NewAutomation {
        name: name.to_string(),
        enabled: true,
        triggers: vec![json!({ "kind": "webhook", "path": "/run" })],
        condition: None,
        actions,
        spec: None,
        grant_id: None,
    }
}

/// A trivial ungated tool that echoes its args — for the code-node host test.
struct EchoTool;

#[async_trait]
impl catalerum_core::tool::Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn invoke(&self, args: Value, _ctx: &ToolContext) -> catalerum_core::Result<Value> {
        Ok(args)
    }
}

/// A tool requiring `notes:write` — exercises the capability gate.
struct GatedTool;

#[async_trait]
impl catalerum_core::tool::Tool for GatedTool {
    fn name(&self) -> &str {
        "gated"
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn required_capability(&self) -> Option<Capability> {
        Some(Capability::new(CapAction::Write, Resource::domain("notes")))
    }
    async fn invoke(&self, _args: Value, _ctx: &ToolContext) -> catalerum_core::Result<Value> {
        Ok(json!({ "ok": true }))
    }
}

/// `ToolActionRunner` as a [`CodeToolHost`] dispatches an allowed tool, rejects
/// an unknown one, and enforces the run's capability cap — the SAME deny-by-default
/// gate an action node uses, so a code node's `callTool` can do no more than the
/// automation's authority permits. DB-free: with no store the authority is the
/// explicitly-set cap set. Each call runs on a blocking thread (as a real code
/// node does) so `call_tool`'s internal `block_on` is valid.
#[tokio::test]
async fn code_tool_host_dispatches_under_authority() {
    use std::sync::Arc;

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(EchoTool));
    registry.register(Arc::new(GatedTool));
    // No store + a bounded cap set (here: empty, so only ungated tools dispatch).
    let runner = ToolActionRunner::new(registry).with_capabilities(Vec::new());
    let ws = WorkspaceId::new();

    // An ungated tool dispatches and its result flows back to the script.
    let echoed = {
        let runner = runner.clone();
        tokio::task::spawn_blocking(move || {
            runner.call_tool(ws, None, "echo", json!({ "x": 1 }))
        })
        .await
        .unwrap()
    };
    assert_eq!(echoed.unwrap(), json!({ "x": 1 }));

    // An unknown tool is rejected before any dispatch.
    let unknown = {
        let runner = runner.clone();
        tokio::task::spawn_blocking(move || runner.call_tool(ws, None, "nope", json!({})))
            .await
            .unwrap()
    };
    assert!(unknown.unwrap_err().contains("unknown tool"));

    // A tool needing a capability beyond the run's authority is denied.
    let denied = {
        let runner = runner.clone();
        tokio::task::spawn_blocking(move || runner.call_tool(ws, None, "gated", json!({})))
            .await
            .unwrap()
    };
    assert!(
        denied.is_err(),
        "gated tool must be denied under empty caps"
    );
}

#[test]
fn tool_mapping_covers_the_tool_backed_kinds_only() {
    assert_eq!(tool_for(ActionKind::CreateNote), Some("create_note"));
    assert_eq!(tool_for(ActionKind::MoveTask), Some("kanban_move_task"));
    assert_eq!(tool_for(ActionKind::CreateEvent), Some("create_event"));
    assert_eq!(tool_for(ActionKind::UpdateEvent), Some("update_event"));
    assert_eq!(tool_for(ActionKind::RunCommand), Some("run_command"));
    assert_eq!(tool_for(ActionKind::OpenTerminal), Some("open_terminal"));
    assert_eq!(tool_for(ActionKind::TerminalWrite), Some("terminal_write"));
    assert_eq!(tool_for(ActionKind::TerminalRead), Some("terminal_read"));
    assert_eq!(
        tool_for(ActionKind::PersistTerminal),
        Some("persist_terminal")
    );
    assert_eq!(tool_for(ActionKind::CloseTerminal), Some("close_terminal"));
    assert_eq!(tool_for(ActionKind::Notify), Some("notify"));
    assert_eq!(tool_for(ActionKind::IndexDocument), Some("index_document"));
    // The object + webhook actions map to their registry tools (SOUL §9/§11/§27).
    assert_eq!(tool_for(ActionKind::WriteObject), Some("write_object"));
    assert_eq!(tool_for(ActionKind::MoveObject), Some("move_object"));
    assert_eq!(tool_for(ActionKind::Webhook), Some("send_webhook"));
    // The LLM kinds are special-cased before `tool_for` (they run the client,
    // not a single tool dispatch), so they have no tool mapping.
    assert_eq!(tool_for(ActionKind::LlmAgent), None);
    assert_eq!(tool_for(ActionKind::Summarize), None);
    assert_eq!(tool_for(ActionKind::CreateChatThread), None);
    // The collect-write actions are special-cased before `tool_for` (they read
    // the whole trigger item + do a store upsert), so they have no tool mapping.
    assert_eq!(tool_for(ActionKind::WriteEmail), None);
    assert_eq!(tool_for(ActionKind::WriteEvent), None);
    assert_eq!(tool_for(ActionKind::LabelEmail), None);
    assert_eq!(tool_for(ActionKind::MarkEmailRead), None);
}

/// A CreateChatThread output node templates an upstream result, atomically
/// creates a visible automation-origin conversation with one assistant
/// message, and returns ids for downstream nodes. The direct store path still
/// enforces the automation's `conversation:write` authority.
#[tokio::test]
async fn create_chat_thread_publishes_automation_output_and_checks_authority() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping create_chat_thread test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = crate::test_db::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create(
            "chat-output",
            &format!("chat-output-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("workspace");
    let owner = store
        .users()
        .create(
            &format!("chat-output-{}@t.test", uuid::Uuid::new_v4()),
            "Owner",
            None,
        )
        .await
        .expect("owner");
    store
        .memberships()
        .upsert(ws.id, owner.id, Role::Owner)
        .await
        .expect("membership");

    let action: Action = serde_json::from_value(json!({
        "kind": "create_chat_thread",
        "title": "Daily {{ inputs.report.day }}",
        "message": "{{ inputs.report.text }}"
    }))
    .unwrap();
    let run_context = json!({
        "trigger": null,
        "inputs": {
            "report": { "day": "Tuesday", "text": "Everything is green." }
        }
    });
    let runner =
        ToolActionRunner::workspace_owner_authority(ToolRegistry::new(), store.clone());
    let outcome = runner.run(ws.id, &action, Some(&run_context), None).await;
    assert_eq!(outcome.status, StepStatus::Succeeded);
    let output = outcome.output.expect("output ids");

    let conversations = store
        .conversations()
        .list_by_workspace(ws.id)
        .await
        .unwrap();
    assert_eq!(conversations.len(), 1);
    let conversation = &conversations[0];
    assert_eq!(conversation.origin, Origin::Automation);
    assert_eq!(conversation.title.as_deref(), Some("Daily Tuesday"));
    assert_eq!(output["conversation_id"], json!(conversation.id));

    let messages = store
        .messages()
        .list_by_conversation(conversation.id)
        .await
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, MessageRole::Assistant);
    assert_eq!(messages[0].content, "Everything is green.");
    assert_eq!(output["message_id"], json!(messages[0].id));

    let denied = runner
        .clone()
        .with_capabilities(Vec::new())
        .run(ws.id, &action, Some(&run_context), None)
        .await;
    assert_eq!(denied.status, StepStatus::Failed);
    assert!(denied
        .error
        .as_deref()
        .is_some_and(|error| error.contains("conversation:write")));
    assert_eq!(
        store
            .conversations()
            .list_by_workspace(ws.id)
            .await
            .unwrap()
            .len(),
        1,
        "denied output must not create another thread"
    );
}

/// End-to-end (SOUL §10/§11/§28): a Maildir with two messages, a graph
/// automation `CollectEmail(conn) → WriteEmail` with `commit_on` on the write,
/// run through [`catalerum_ingest::run_collect_email`] — one run fires per
/// message, each `WriteEmail` upserts the message, the `commit_on` write
/// `Succeeded` so both commit, and a second poll of the unchanged mailbox
/// collects nothing (the cursor advanced over the committed prefix).
#[tokio::test]
async fn collect_email_fires_per_message_writes_them_and_commits_then_is_idempotent() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping collect_email e2e: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    use catalerum_core::model::ConnectionKind;
    use catalerum_ingest::{run_collect_email, AutomationContext, CollectPayload};
    use std::sync::Arc;

    // Use an ISOLATED throwaway db (own automations table), NOT the shared dev DB:
    // this test creates an *enabled* CollectEmail automation, and a dev server's
    // ScheduleWorker scans every workspace each tick — a leaked one would make it
    // enqueue a `collect_email` job forever (the "why is email still running" bug).
    let store = crate::test_db::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("collect", &format!("collect-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    // workspace_owner_authority resolves the owner's base-Member caps (which
    // include email:write), so the workspace needs an Owner.
    let owner = store
        .users()
        .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
        .await
        .expect("owner");
    store
        .memberships()
        .upsert(ws.id, owner.id, Role::Owner)
        .await
        .expect("membership");

    // A temp Maildir with two unseen messages in new/.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("new")).unwrap();
    std::fs::create_dir_all(dir.path().join("cur")).unwrap();
    let msg = |from: &str, subject: &str, body: &str| {
        format!(
            "From: {from}\r\nTo: me@example.com\r\nSubject: {subject}\r\nDate: Wed, 01 Jan 2026 10:00:00 +0000\r\nMessage-ID: <{subject}@x>\r\n\r\n{body}\r\n"
        )
    };
    std::fs::write(
        dir.path().join("new/msg-1"),
        msg("Ada <ada@bank.com>", "stmt-one", "your statement"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("new/msg-2"),
        msg("Bob <bob@shop.com>", "receipt-two", "a refund"),
    )
    .unwrap();

    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Email,
            "maildir",
            None,
            Some(json!({ "provider": "maildir", "root": dir.path().to_str().unwrap(), "name": "INBOX" })),
        )
        .await
        .expect("conn");

    // Graph: CollectEmail(conn) → WriteEmail, cursor committed on the write. A
    // wide `backfill_window` so the fixture's dated messages aren't dropped by
    // the default first-poll grace (which collects only very-recent mail).
    let collect_trigger = json!({
        "kind": "collect_email",
        "connection": conn.id.to_string(),
        "commit_on": "w",
        "backfill_window": { "days": 3650 }
    });
    let spec = json!({ "graph": {
        "nodes": [
            { "id": "c", "kind": "trigger", "trigger": collect_trigger },
            { "id": "w", "kind": "action", "action": { "kind": "write_email" } }
        ],
        "edges": [ { "from": "c", "to": "w" } ]
    }});
    let automation = store
        .automations()
        .create(
            ws.id,
            &NewAutomation {
                name: "collect-mail".into(),
                enabled: true,
                triggers: vec![collect_trigger.clone()],
                condition: None,
                actions: vec![],
                spec: Some(spec),
                grant_id: None,
            },
        )
        .await
        .expect("automation");

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let runner: Arc<dyn ActionRunner> = Arc::new(ToolActionRunner::workspace_owner_authority(
        registry,
        store.clone(),
    ));
    let ctx = AutomationContext::new(runner);
    let payload = CollectPayload::new(ws.id, automation.id, collect_trigger.clone());

    // First poll: one run per message, both written, both committed.
    let report = run_collect_email(&store, &ctx, ws.id, &payload)
        .await
        .expect("collect");
    assert_eq!(report.runs_fired, 2, "one run per new message");
    assert_eq!(
        report.committed, 2,
        "the commit_on WriteEmail succeeded for both"
    );
    assert_eq!(report.sources, 1);

    let stored = store.emails().list_by_workspace(ws.id, 50).await.unwrap();
    assert_eq!(stored.len(), 2, "WriteEmail persisted both messages");
    let subjects: std::collections::HashSet<&str> =
        stored.iter().map(|e| e.subject.as_str()).collect();
    assert!(subjects.contains("stmt-one") && subjects.contains("receipt-two"));

    // Second poll of the unchanged Maildir: nothing new (cursor advanced over the
    // committed prefix; the content-hash cursor short-circuits an unchanged snapshot).
    let again = run_collect_email(&store, &ctx, ws.id, &payload)
        .await
        .expect("collect again");
    assert_eq!(
        again.runs_fired, 0,
        "an unchanged mailbox collects nothing on re-poll"
    );
    assert_eq!(
        store
            .emails()
            .list_by_workspace(ws.id, 50)
            .await
            .unwrap()
            .len(),
        2,
        "no duplicate emails on re-poll"
    );

    // A THIRD message arrives → the snapshot changes and is re-emitted in FULL
    // (all 3 uids). Only the genuinely-new message fires a run — the two already
    // committed are skipped (the ledger survived the no-change poll; this is the
    // snapshot double-run regression the review caught).
    std::fs::write(
        dir.path().join("new/msg-3"),
        msg("Carol <carol@x.com>", "third-msg", "hi"),
    )
    .unwrap();
    let third = run_collect_email(&store, &ctx, ws.id, &payload)
        .await
        .expect("collect third");
    assert_eq!(
        third.runs_fired, 1,
        "only the new message re-fires, not the whole snapshot"
    );
    assert_eq!(third.committed, 1);
    assert_eq!(
        store
            .emails()
            .list_by_workspace(ws.id, 50)
            .await
            .unwrap()
            .len(),
        3,
        "the third message is now stored; the first two were not re-written"
    );
}

/// `WriteEmail` reports the `newly_written` redelivery signal (SOUL §11/§29): the
/// first write of a `(mailbox_id, uid)` is `true`, and a **redelivery** (a second
/// write of the same key — the crash-window replay) is `false`. That `false` is
/// what the DAG executor latches to auto-skip a downstream `LlmAgent`/`LabelEmail`,
/// so this test pins the durable signal at its source.
#[tokio::test]
async fn write_email_reports_newly_written_true_then_false_on_redelivery() {
    use catalerum_core::model::ConnectionKind;

    let Some(url) = db_url() else {
        eprintln!(
            "skipping newly_written test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = crate::test_db::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("nw", &format!("nw-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let owner = store
        .users()
        .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
        .await
        .expect("owner");
    store
        .memberships()
        .upsert(ws.id, owner.id, Role::Owner)
        .await
        .expect("membership");
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Email,
            "maildir",
            None,
            Some(json!({ "provider": "maildir", "name": "INBOX" })),
        )
        .await
        .expect("conn");
    // A real mailbox row so the WriteEmail upsert's FK is satisfied.
    let mb = store
        .mailboxes()
        .upsert(ws.id, conn.id, "INBOX", "INBOX", false)
        .await
        .expect("mailbox");

    // A collected email carried on the firing trigger (graph form: trigger.item).
    let item = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "workspace_id": ws.id.to_string(),
        "mailbox_id": mb.id.to_string(),
        "uid": "u-redeliv-1",
        "subject": "your statement",
        "has_attachments": false
    });
    let trigger = json!({ "trigger": { "item": item } });
    let action: Action = serde_json::from_value(json!({ "kind": "write_email" })).unwrap();

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());

    // First delivery → the row is inserted → newly_written = true.
    let first = runner.run(ws.id, &action, Some(&trigger), None).await;
    assert_eq!(
        first.status,
        StepStatus::Succeeded,
        "first write succeeds: {:?}",
        first.error
    );
    assert_eq!(
        first.output.as_ref().unwrap()["newly_written"],
        json!(true),
        "the first write of a (mailbox_id, uid) is newly written"
    );

    // Redelivery of the same item → the upsert finds the existing row → false.
    let again = runner.run(ws.id, &action, Some(&trigger), None).await;
    assert_eq!(again.status, StepStatus::Succeeded);
    assert_eq!(
        again.output.as_ref().unwrap()["newly_written"],
        json!(false),
        "a redelivery of an already-stored message reports newly_written=false"
    );
    // Idempotent: still exactly one stored email for that uid.
    assert_eq!(
        store
            .emails()
            .list_by_workspace(ws.id, 50)
            .await
            .unwrap()
            .len(),
        1,
        "the redelivery upsert did not duplicate the message"
    );
}

/// Archival (SOUL §9/§28/§29 — the §29 "attachments: bucket + link" resolution +
/// the raw-`.eml` deferral): `archive_email` writes the raw message + each
/// attachment as **objects** in the workspace's files store and links them onto
/// the row; a redelivery re-archives nothing; and `cleanup_email_archive` (the
/// deletion reconcile) removes the objects. Uses a local-FS store + live Postgres.
#[tokio::test]
async fn archive_email_writes_objects_links_refs_is_idempotent_and_cleans_up() {
    use catalerum_core::model::ConnectionKind;
    use catalerum_storage::LocalFsBackend;

    let Some(url) = db_url() else {
        eprintln!(
            "skipping archive_email test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = crate::test_db::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("arch", &format!("arch-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let owner = store
        .users()
        .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
        .await
        .expect("owner");
    store
        .memberships()
        .upsert(ws.id, owner.id, Role::Owner)
        .await
        .expect("membership");
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Email,
            "maildir",
            None,
            Some(json!({ "provider": "maildir", "name": "INBOX" })),
        )
        .await
        .expect("conn");
    let mb = store
        .mailboxes()
        .upsert(ws.id, conn.id, "INBOX", "INBOX", false)
        .await
        .expect("mailbox");
    // A stored email row for archive_email to find + link onto.
    let mut email: Email = serde_json::from_value(json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "workspace_id": ws.id.to_string(),
        "mailbox_id": mb.id.to_string(),
        "uid": "u-arch-1",
        "subject": "with attachment",
        "has_attachments": true
    }))
    .unwrap();
    email.workspace_id = ws.id;
    let stored = store
        .emails()
        .upsert_by_uid(&email)
        .await
        .expect("upsert email");
    assert!(stored.raw_ref.is_none() && stored.attachments.is_empty());

    // A local-FS files store as the default, threaded onto the runner.
    let tmp = tempfile::tempdir().expect("tmp");
    let backend: Arc<dyn catalerum_core::provider::StorageBackend> =
        Arc::new(LocalFsBackend::new(tmp.path().to_path_buf()));
    let config_store = crate::state::ConfigStore {
        backend: backend.clone(),
        connection: "files".to_string(),
        bucket: "files".to_string(),
        kind: "local",
        namespaced: true,
        workspaces: Vec::new(),
    };
    let registry = crate::state::StorageRegistry::single_for_test("files", config_store);
    let tool_registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let runner = ToolActionRunner::workspace_owner_authority(tool_registry, store.clone())
        .with_storage(Arc::new(registry.clone()));

    // Archive: a raw .eml + one attachment (bytes passed directly, as the collect
    // pipeline would after MIME-extracting them).
    let raw = b"From: a@x.com\r\nSubject: with attachment\r\n\r\nbody\r\n".to_vec();
    let atts = vec![ExtractedAttachment {
        filename: Some("invoice.pdf".to_string()),
        content_type: Some("application/pdf".to_string()),
        data: b"%PDF-1.4 fake".to_vec(),
    }];
    runner
        .archive_email(ws.id, mb.id, "u-arch-1", Some(raw.clone()), atts)
        .await;

    // The row now links the archived objects, under the stable key prefix.
    let after = store.emails().get(ws.id, stored.id).await.expect("reload");
    assert_eq!(
        after.raw_ref.as_deref(),
        Some(&*format!("emails/{}/u-arch-1/raw.eml", mb.id)),
        "raw_ref points at the archived .eml key"
    );
    assert_eq!(after.attachments.len(), 1, "one attachment ref");
    let att_ref = &after.attachments[0];
    assert_eq!(
        att_ref.url,
        format!(
            "/storage/objects/emails/{}/u-arch-1/attachments/0-invoice.pdf",
            mb.id
        )
    );
    assert_eq!(att_ref.filename.as_deref(), Some("invoice.pdf"));
    assert_eq!(att_ref.content_type.as_deref(), Some("application/pdf"));

    // The blobs physically exist on the backend (namespaced under <ws>/).
    let handle = registry.get("files").unwrap().handle("files".to_string());
    let raw_phys = handle.physical_key(ws.id, after.raw_ref.as_deref().unwrap());
    let att_key = att_ref.url.strip_prefix("/storage/objects/").unwrap();
    let att_phys = handle.physical_key(ws.id, att_key);
    assert!(
        backend.stat(&raw_phys).await.is_ok(),
        "raw .eml blob exists"
    );
    assert!(
        backend.stat(&att_phys).await.is_ok(),
        "attachment blob exists"
    );

    // Redelivery: archiving the same message again is a no-op (row already
    // archived) — refs unchanged, no duplicate objects.
    runner
        .archive_email(ws.id, mb.id, "u-arch-1", Some(raw), vec![])
        .await;
    let after2 = store.emails().get(ws.id, stored.id).await.expect("reload2");
    assert_eq!(
        after2.raw_ref, after.raw_ref,
        "redelivery leaves raw_ref unchanged"
    );
    assert_eq!(
        after2.attachments.len(),
        1,
        "redelivery adds no attachment refs"
    );

    // Deletion reconcile: cleanup removes the archived blobs.
    let keys = vec![after.raw_ref.clone().unwrap(), att_key.to_string()];
    runner.cleanup_email_archive(ws.id, keys).await;
    assert!(
        backend.stat(&raw_phys).await.is_err(),
        "raw .eml blob deleted on cleanup"
    );
    assert!(
        backend.stat(&att_phys).await.is_err(),
        "attachment blob deleted on cleanup"
    );
}

/// `WriteEvent` reports the `newly_written` redelivery signal (SOUL §11/§29) — the
/// calendar twin of [`write_email_reports_newly_written_true_then_false_on_redelivery`]:
/// the first write of a `(calendar_id, uid)` is `true`, a redelivery (a second write
/// of the same key) is `false`. That `false` is what the DAG executor latches to
/// auto-skip a downstream `LlmAgent` in a `CollectCalendar → WriteEvent → LlmAgent`
/// flow, so this pins the durable signal at its source (via `EventRepo::get_by_uid`).
#[tokio::test]
async fn write_event_reports_newly_written_true_then_false_on_redelivery() {
    use catalerum_core::model::ConnectionKind;

    let Some(url) = db_url() else {
        eprintln!("skipping write_event newly_written test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = crate::test_db::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("nwe", &format!("nwe-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let owner = store
        .users()
        .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
        .await
        .expect("owner");
    store
        .memberships()
        .upsert(ws.id, owner.id, Role::Owner)
        .await
        .expect("membership");
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "caldav",
            None,
            Some(json!({ "provider": "caldav" })),
        )
        .await
        .expect("conn");
    // A real calendar row so the WriteEvent upsert's FK is satisfied.
    let cal = store
        .calendars()
        .upsert(ws.id, conn.id, "ext-cal", "Work", false)
        .await
        .expect("calendar");

    // A collected event carried on the firing trigger (graph form: trigger.item).
    let now = chrono::Utc::now();
    let item = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "workspace_id": ws.id.to_string(),
        "calendar_id": cal.id.to_string(),
        "uid": "u-ev-redeliv-1",
        "start": now.to_rfc3339(),
        "end": (now + chrono::Duration::hours(1)).to_rfc3339(),
        "summary": "Standup",
        "sequence": 0
    });
    let trigger = json!({ "trigger": { "item": item } });
    let action: Action = serde_json::from_value(json!({ "kind": "write_event" })).unwrap();

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());

    // First delivery → the row is inserted → newly_written = true.
    let first = runner.run(ws.id, &action, Some(&trigger), None).await;
    assert_eq!(
        first.status,
        StepStatus::Succeeded,
        "first write succeeds: {:?}",
        first.error
    );
    assert_eq!(
        first.output.as_ref().unwrap()["newly_written"],
        json!(true),
        "the first write of a (calendar_id, uid) is newly written"
    );

    // Redelivery of the same item → the upsert finds the existing row → false.
    let again = runner.run(ws.id, &action, Some(&trigger), None).await;
    assert_eq!(again.status, StepStatus::Succeeded);
    assert_eq!(
        again.output.as_ref().unwrap()["newly_written"],
        json!(false),
        "a redelivery of an already-stored event reports newly_written=false"
    );
    // Idempotent: still exactly one stored event for that uid.
    let events = store
        .events()
        .list_by_workspace(
            ws.id,
            None,
            catalerum_store::DateRange {
                from: None,
                to: None,
            },
            50,
        )
        .await
        .unwrap();
    assert_eq!(
        events.len(),
        1,
        "the redelivery upsert did not duplicate the event"
    );
}

/// `WriteEvent`'s optional `calendar_id` param redirects the write into a
/// specific **local** calendar (overriding the collected item's source
/// calendar), and refuses a provider calendar — a row planted in a provider
/// mirror would silently vanish on its next snapshot sync (SOUL §8).
#[tokio::test]
async fn write_event_calendar_id_param_redirects_into_a_local_calendar_only() {
    use catalerum_core::model::ConnectionKind;

    let Some(url) = db_url() else {
        eprintln!("skipping write_event calendar_id test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = crate::test_db::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("wecal", &format!("wecal-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let owner = store
        .users()
        .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
        .await
        .expect("owner");
    store
        .memberships()
        .upsert(ws.id, owner.id, Role::Owner)
        .await
        .expect("membership");
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "caldav",
            None,
            Some(json!({ "provider": "caldav" })),
        )
        .await
        .expect("conn");
    // The collect source's mirror (provider) calendar and the redirect target.
    let source = store
        .calendars()
        .upsert(ws.id, conn.id, "ext-cal", "Work", false)
        .await
        .expect("source calendar");
    let local = store
        .calendars()
        .upsert_local(ws.id, "redirect", "Planning")
        .await
        .expect("local calendar");

    let now = chrono::Utc::now();
    let item = json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "workspace_id": ws.id.to_string(),
        "calendar_id": source.id.to_string(),
        "uid": "u-ev-redirect-1",
        "start": now.to_rfc3339(),
        "end": (now + chrono::Duration::hours(1)).to_rfc3339(),
        "summary": "Planning sync",
        "sequence": 0
    });
    let trigger = json!({ "trigger": { "item": item } });

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());

    // Redirect into the local calendar: the stored row lands there, not in
    // the source mirror.
    let action: Action = serde_json::from_value(json!({
        "kind": "write_event",
        "calendar_id": local.id.to_string()
    }))
    .unwrap();
    let out = runner.run(ws.id, &action, Some(&trigger), None).await;
    assert_eq!(
        out.status,
        StepStatus::Succeeded,
        "redirected write succeeds: {:?}",
        out.error
    );
    assert_eq!(
        out.output.as_ref().unwrap()["calendar_id"],
        json!(local.id.to_string()),
        "the event was written into the redirect target, not the source calendar"
    );

    // A provider calendar as the target is refused.
    let action: Action = serde_json::from_value(json!({
        "kind": "write_event",
        "calendar_id": source.id.to_string()
    }))
    .unwrap();
    let out = runner.run(ws.id, &action, Some(&trigger), None).await;
    assert_eq!(out.status, StepStatus::Failed);
    assert!(
        out.error
            .as_deref()
            .unwrap_or("")
            .contains("local calendar"),
        "the failure names the local-calendar restriction: {:?}",
        out.error
    );

    // A calendar id from another workspace (or a bogus one) is refused too.
    let action: Action = serde_json::from_value(json!({
        "kind": "write_event",
        "calendar_id": uuid::Uuid::new_v4().to_string()
    }))
    .unwrap();
    let out = runner.run(ws.id, &action, Some(&trigger), None).await;
    assert_eq!(out.status, StepStatus::Failed);
    assert!(
        out.error.as_deref().unwrap_or("").contains("not found"),
        "an unknown calendar id fails with not-found: {:?}",
        out.error
    );
}

/// `WriteEvent`'s `calendar` param names a **local** calendar to write into:
/// an existing one of that name is reused, and a not-yet-existing name is
/// created on the fly — so an automation can target e.g. "Feiertage Bayern"
/// without a separate create_calendar step. Idempotent across runs.
#[tokio::test]
async fn write_event_calendar_name_reuses_or_creates_a_local_calendar() {
    use catalerum_core::model::ConnectionKind;

    let Some(url) = db_url() else {
        eprintln!("skipping write_event calendar-name test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = crate::test_db::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("wecaln", &format!("wecaln-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let owner = store
        .users()
        .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
        .await
        .expect("owner");
    store
        .memberships()
        .upsert(ws.id, owner.id, Role::Owner)
        .await
        .expect("membership");
    // The collect source's mirror (a provider calendar named "bavaria", like
    // the reported ICS-holidays case).
    let conn = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "webcal",
            None,
            Some(json!({ "provider": "caldav" })),
        )
        .await
        .expect("conn");
    let source = store
        .calendars()
        .upsert(ws.id, conn.id, "ext-cal", "bavaria", true)
        .await
        .expect("source calendar");

    let now = chrono::Utc::now();
    let mk_item = |uid: &str| {
        json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "workspace_id": ws.id.to_string(),
            "calendar_id": source.id.to_string(),
            "uid": uid,
            "start": now.to_rfc3339(),
            "end": (now + chrono::Duration::hours(1)).to_rfc3339(),
            "summary": "Neujahr",
            "sequence": 0
        })
    };

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());

    // First write to a name that does not exist yet → a local calendar of
    // that name is created and the event lands there, NOT in the source.
    let action: Action = serde_json::from_value(json!({
        "kind": "write_event",
        "calendar": "Feiertage Bayern"
    }))
    .unwrap();
    let trigger = json!({ "trigger": { "item": mk_item("u-fb-1") } });
    let out = runner.run(ws.id, &action, Some(&trigger), None).await;
    assert_eq!(
        out.status,
        StepStatus::Succeeded,
        "named write succeeds: {:?}",
        out.error
    );
    let created_id = out.output.as_ref().unwrap()["calendar_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(
        created_id,
        source.id.to_string(),
        "the event did not land in the collect source (\"bavaria\")"
    );
    let created: catalerum_core::CalendarId = created_id.parse().unwrap();
    let cal = store.calendars().get(ws.id, created).await.expect("cal");
    assert_eq!(cal.name, "Feiertage Bayern");
    assert!(cal.is_local(), "target is a local calendar");

    // A second write of a *different* event to the SAME name reuses that same
    // calendar (idempotent — no per-event duplicate calendars).
    let trigger = json!({ "trigger": { "item": mk_item("u-fb-2") } });
    let out = runner.run(ws.id, &action, Some(&trigger), None).await;
    assert_eq!(out.status, StepStatus::Succeeded);
    assert_eq!(
        out.output.as_ref().unwrap()["calendar_id"],
        json!(created_id),
        "the same name resolves to the same local calendar on re-run"
    );
    let locals = store
        .calendars()
        .list_by_workspace(ws.id)
        .await
        .expect("list")
        .into_iter()
        .filter(|c| c.is_local() && c.name == "Feiertage Bayern")
        .count();
    assert_eq!(locals, 1, "no duplicate \"Feiertage Bayern\" calendars");
}

/// FULL end-to-end (the reported bug): a `CollectCalendar` over a local
/// `.ics` of **all-day** holidays (like the Bavaria feed) → a `WriteEvent`
/// that names its destination calendar ("Feiertage Bayern") actually PERSISTS
/// the events into a freshly-created local calendar — proving the whole
/// collect→write chain writes (the earlier failure: WriteEvent redirected to a
/// calendar id that wasn't a writable local one, so every write failed and
/// nothing landed anywhere).
#[tokio::test]
async fn collect_calendar_all_day_writes_events_into_a_named_local_calendar() {
    use std::sync::Arc;

    use catalerum_automation::ActionRunner;
    use catalerum_core::model::ConnectionKind;
    use catalerum_ingest::{run_collect_calendar, AutomationContext, CollectPayload};

    // All-day (`VALUE=DATE`) events, far-future so they always clear the
    // first-poll 1-day backfill cutoff regardless of the test clock — the
    // exact shape of the officeholidays Bavaria feed.
    const ICS: &str = "BEGIN:VCALENDAR\r
VERSION:2.0\r
PRODID:-//test//bavaria//EN\r
BEGIN:VEVENT\r
UID:assumption@bayern\r
DTSTART;VALUE=DATE:20990815\r
DTEND;VALUE=DATE:20990816\r
SUMMARY:Bavaria: Assumption Day\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:unity@bayern\r
DTSTART;VALUE=DATE:20991003\r
DTEND;VALUE=DATE:20991004\r
SUMMARY:Bavaria: German Unity Day\r
END:VEVENT\r
BEGIN:VEVENT\r
UID:christmas@bayern\r
DTSTART;VALUE=DATE:20991225\r
DTEND;VALUE=DATE:20991226\r
SUMMARY:Bavaria: Christmas Day\r
END:VEVENT\r
END:VCALENDAR\r
";

    let Some(url) = db_url() else {
        eprintln!(
            "skipping collect->write e2e test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = crate::test_db::isolated_store(&url).await;
    let ws = store
        .workspaces()
        .create("bayern", &format!("bayern-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    // The runner resolves the workspace owner's authority (base-Member caps
    // cover calendar:read/write), so an Owner membership is required.
    let owner = store
        .users()
        .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
        .await
        .expect("owner");
    store
        .memberships()
        .upsert(ws.id, owner.id, Role::Owner)
        .await
        .expect("membership");

    // A local `.ics` calendar connection over a temp dir holding the feed.
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("bavaria.ics"), ICS).unwrap();
    let connection = store
        .connections()
        .create(
            ws.id,
            ConnectionKind::Calendar,
            "ics",
            None,
            Some(json!({ "provider": "local", "path": dir.path().to_string_lossy() })),
        )
        .await
        .expect("connection");
    let conn = connection.id.to_string();

    // The user's automation: collect the feed → write each event into a
    // calendar named "Feiertage Bayern" (which does NOT exist yet).
    let automation = store
        .automations()
        .create(
            ws.id,
            &NewAutomation {
                name: "bayern-holidays".into(),
                enabled: true,
                triggers: vec![json!({ "kind": "collect_calendar", "connection": conn })],
                condition: None,
                actions: vec![json!({
                    "kind": "write_event",
                    "calendar": "Feiertage Bayern"
                })],
                spec: None,
                grant_id: None,
            },
        )
        .await
        .expect("automation");

    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let runner: Arc<dyn ActionRunner> = Arc::new(ToolActionRunner::workspace_owner_authority(
        registry,
        store.clone(),
    ));
    let ctx = AutomationContext::new(runner);

    let payload = CollectPayload::new(ws.id, automation.id, automation.triggers[0].clone());
    let report = run_collect_calendar(&store, &ctx, ws.id, &payload)
        .await
        .expect("collect poll");
    assert_eq!(report.sources, 1, "the one .ics file is the one source");
    assert_eq!(report.runs_fired, 3, "one run fired per all-day event");

    // The destination calendar was auto-created, local, and holds all three
    // events — none of which leaked into the collect source ("bavaria.ics").
    let calendars = store.calendars().list_by_workspace(ws.id).await.unwrap();
    let target = calendars
        .iter()
        .find(|c| c.is_local() && c.name == "Feiertage Bayern")
        .expect("the named destination calendar was created");
    let source = calendars
        .iter()
        .find(|c| !c.is_local())
        .expect("the collect source calendar exists");
    for uid in ["assumption@bayern", "unity@bayern", "christmas@bayern"] {
        let ev = store
            .events()
            .get_by_uid(ws.id, target.id, uid)
            .await
            .unwrap_or_else(|e| panic!("event {uid} written to Feiertage Bayern: {e}"));
        assert!(ev.all_day, "the all-day flag survives the write");
        assert!(
            matches!(
                store.events().get_by_uid(ws.id, source.id, uid).await,
                Err(catalerum_store::StoreError::NotFound)
            ),
            "event {uid} must NOT have landed in the collect source calendar"
        );
    }
}

#[test]
fn local_calendar_external_id_is_a_stable_prefixed_slug() {
    assert_eq!(
        local_calendar_external_id("Feiertage Bayern"),
        "named-feiertage-bayern"
    );
    // Same name (case/space-insensitive at the edges) → same key.
    assert_eq!(
        local_calendar_external_id("  Feiertage   Bayern  "),
        local_calendar_external_id("Feiertage   Bayern")
    );
    // Never collides with the reserved auto-default calendar key ("default").
    assert_ne!(local_calendar_external_id("default"), "default");
    // Punctuation-only / empty names still yield a valid non-empty key.
    assert_eq!(local_calendar_external_id("!!!"), "named-cal");
}

#[test]
fn render_params_threads_upstream_outputs_into_action_params() {
    // The graph-run context: the firing trigger + an upstream `open` node's
    // output (the shape a `terminal_write` node sees).
    let ctx = json!({
        "trigger": { "kind": "webhook", "path": "/convert" },
        "inputs": {
            "open": { "session_id": "abc-123", "kind": "ephemeral" },
            "gen":  { "data": { "lines": 7 } }
        }
    });
    let params: serde_json::Map<String, Value> = json!({
        "session_id": "{{ inputs.open.session_id }}",
        "data": "python convert.py # {{ inputs.gen.data.lines }} lines\n",
        "count": "{{ inputs.gen.data.lines }}",
        "literal": "no templates here",
        "kept": "{{ inputs.missing.field }}"
    })
    .as_object()
    .unwrap()
    .clone();

    let out = render_params(&params, Some(&ctx));
    // Whole-string template → the resolved string, verbatim (no surrounding text).
    assert_eq!(out["session_id"], json!("abc-123"));
    // Whole-string template over a number → the JSON number, type preserved.
    assert_eq!(out["count"], json!(7));
    // Embedded template → stringified + interpolated into the surrounding text.
    assert_eq!(out["data"], json!("python convert.py # 7 lines\n"));
    // No template → unchanged.
    assert_eq!(out["literal"], json!("no templates here"));
    // Unresolved path → left verbatim (a clear downstream error, not silent loss).
    assert_eq!(out["kept"], json!("{{ inputs.missing.field }}"));
}

#[test]
fn render_params_is_a_noop_without_context_and_resolves_arrays() {
    let params: serde_json::Map<String, Value> =
        json!({ "x": "{{ a }}" }).as_object().unwrap().clone();
    // No context → params returned unchanged (the linear/manual path).
    assert_eq!(render_params(&params, None)["x"], json!("{{ a }}"));

    // Array indexing + nested rendering inside an array param.
    let ctx = json!({ "inputs": { "n": { "items": ["alpha", "beta"] } } });
    let p: serde_json::Map<String, Value> =
        json!({ "argv": ["echo", "{{ inputs.n.items.1 }}"] })
            .as_object()
            .unwrap()
            .clone();
    let out = render_params(&p, Some(&ctx));
    assert_eq!(out["argv"], json!(["echo", "beta"]));
}

#[tokio::test]
async fn automation_creates_a_note_through_the_registry_capability_enforced() {
    let Some(url) = db_url() else {
        eprintln!(
            "skipping tool-action-runner test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("autorun", &format!("autorun-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    // embed/graph off → only Postgres needed for create_note.
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let user = UserId::new();

    // 1. A Member-authority automation that creates a note → run Succeeded, and
    //    the note really lands in the workspace.
    let make_note = store
        .automations()
        .create(ws.id, &automation("note-bot", vec![json!({
            "kind": "create_note", "title": "From automation", "markdown": "hi", "tags": ["auto"]
        })]))
        .await
        .unwrap();
    let runner = ToolActionRunner::new(registry.clone())
        .as_user(user)
        .with_capabilities(catalerum_iam::base_capabilities(Role::Member));
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &make_note, None)
        .await
        .unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    let notes = store
        .notes()
        .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
        .await
        .unwrap();
    assert!(
        notes.iter().any(|n| n.title == "From automation"),
        "the automation created the note"
    );

    // 2. Viewer authority lacks Write@notes → the action is denied → run Failed.
    let blocked = store
        .automations()
        .create(
            ws.id,
            &automation(
                "note-bot-viewer",
                vec![json!({
                    "kind": "create_note", "title": "blocked"
                })],
            ),
        )
        .await
        .unwrap();
    let viewer = ToolActionRunner::new(registry.clone())
        .as_user(user)
        .with_capabilities(catalerum_iam::base_capabilities(Role::Viewer));
    let run = execute(&store, &viewer, &FailCodeRunner, ws.id, &blocked, None)
        .await
        .unwrap();
    assert_eq!(
        run.status,
        RunStatus::Failed,
        "deny-by-default applies to automations too"
    );
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert_eq!(steps[0].status, StepStatus::Failed);
    // The note was NOT created.
    assert!(!store
        .notes()
        .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
        .await
        .unwrap()
        .iter()
        .any(|n| n.title == "blocked"));

    // 3. An LLM-backed action (`summarize`) on a runner with no LLM client →
    //    run Failed with a clear message. (`llm_agent`/`run_skill` behave the
    //    same; every tool-less kind now has a real runner, so "no tool runner
    //    yet" is gone — a missing dependency is what fails.)
    let unsupported = store
        .automations()
        .create(
            ws.id,
            &automation(
                "summary-bot",
                vec![json!({ "kind": "summarize", "input": "text" })],
            ),
        )
        .await
        .unwrap();
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &unsupported, None)
        .await
        .unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert!(steps[0]
        .error
        .as_deref()
        .unwrap()
        .contains("needs an LLM client"));
}

/// A canned executor so `run_command` is registered (it needs `exec:run`).
struct FakeExec;
#[async_trait]
impl catalerum_core::provider::Executor for FakeExec {
    async fn run(
        &self,
        cmd: catalerum_core::provider::CommandSpec,
    ) -> catalerum_core::error::Result<catalerum_core::provider::CommandResult> {
        Ok(catalerum_core::provider::CommandResult {
            exit_code: 0,
            stdout: cmd.argv.join(" "),
            stderr: String::new(),
            timed_out: false,
        })
    }
    async fn open_session(
        &self,
        _spec: catalerum_core::provider::SessionSpec,
    ) -> catalerum_core::error::Result<catalerum_core::provider::Session> {
        Err(catalerum_core::error::Error::Unsupported(
            "no sessions".into(),
        ))
    }
}

/// §19 enforcement: an automation **without** a grant runs under base-Member and
/// is denied a protected action (`run_command` needs `exec:run`, which no base
/// role holds); **with** a grant conferring `exec:run`, the same action runs.
#[tokio::test]
async fn grant_elevates_an_automation_above_its_base_authority() {
    use catalerum_core::capability::{Action, Capability, Constraints, Resource};
    use std::sync::Arc;

    let Some(url) = db_url() else {
        eprintln!(
            "skipping grant-enforcement test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("grantrun", &format!("grantrun-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    // Registry WITH an executor → `run_command` is registered.
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        Some(Arc::new(FakeExec)),
        Vec::new(),
        None,
        None,
    );
    let user = UserId::new();
    // Default authority is base **Member**, which does NOT hold `exec:run`.
    let runner = ToolActionRunner::new(registry)
        .as_user(user)
        .with_capabilities(catalerum_iam::base_capabilities(Role::Member));

    let cmd = json!({ "kind": "run_command", "command": ["echo", "granted"] });

    // 1. No grant → run_command denied (deny-by-default) → run Failed.
    let ungranted = store
        .automations()
        .create(ws.id, &automation("no-grant", vec![cmd.clone()]))
        .await
        .unwrap();
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &ungranted, None)
        .await
        .unwrap();
    assert_eq!(
        run.status,
        RunStatus::Failed,
        "base Member lacks exec:run → denied"
    );
    assert_eq!(
        run.grant_id, None,
        "an ungranted run records no audit grant"
    );

    // 2. A grant conferring exec:run, attached to the automation → the SAME action
    //    now runs → run Succeeded. The grant elevated the automation's authority.
    let grant = store
        .grants()
        .upsert(
            ws.id,
            "exec-powers",
            &[Capability::new(Action::Run, Resource::domain("exec"))],
            &Default::default(),
        )
        .await
        .unwrap();
    let granted = store
        .automations()
        .create(
            ws.id,
            &catalerum_store::NewAutomation {
                name: "with-grant".into(),
                enabled: true,
                triggers: vec![json!({ "kind": "webhook", "path": "/run" })],
                condition: None,
                actions: vec![cmd],
                spec: None,
                grant_id: Some(grant.id),
            },
        )
        .await
        .unwrap();
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &granted, None)
        .await
        .unwrap();
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "the grant conferred exec:run → the action ran"
    );
    assert_eq!(
        run.grant_id,
        Some(grant.id),
        "the production run path snapshots the authorizing grant for audit (§19)"
    );

    // 3. A `dry_run` grant: the action is AUTHORIZED then SIMULATED at dispatch —
    //    the run Succeeds but the side effect is NOT committed (no note created).
    let dry = store
        .grants()
        .upsert(
            ws.id,
            "dry-run-powers",
            &[Capability::new(Action::Write, Resource::domain("notes"))],
            &Constraints {
                dry_run: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let auto3 = store
        .automations()
        .create(
            ws.id,
            &catalerum_store::NewAutomation {
                name: "dry".into(),
                enabled: true,
                triggers: vec![json!({ "kind": "webhook", "path": "/run" })],
                condition: None,
                actions: vec![json!({
                    "kind": "create_note", "title": "DRYRUN-MARKER", "markdown": "x"
                })],
                spec: None,
                grant_id: Some(dry.id),
            },
        )
        .await
        .unwrap();
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &auto3, None)
        .await
        .unwrap();
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "dry-run authorizes + simulates → run succeeds"
    );
    let notes = store
        .notes()
        .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
        .await
        .unwrap();
    assert!(
        !notes.iter().any(|n| n.title == "DRYRUN-MARKER"),
        "dry-run must NOT commit the side effect — no note created"
    );

    // 4. A grant with a constraint the runtime *still* can't enforce (`rate_limit`)
    //    → fails closed, never running with that guardrail silently dropped.
    let capped = store
        .grants()
        .upsert(
            ws.id,
            "rate-capped",
            &[Capability::new(Action::Write, Resource::domain("notes"))],
            &Constraints {
                rate_limit: Some(5),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let auto4 = store
        .automations()
        .create(
            ws.id,
            &catalerum_store::NewAutomation {
                name: "rate-limited".into(),
                enabled: true,
                triggers: vec![json!({ "kind": "webhook", "path": "/run" })],
                condition: None,
                actions: vec![json!({ "kind": "create_note", "title": "rate-limited" })],
                spec: None,
                grant_id: Some(capped.id),
            },
        )
        .await
        .unwrap();
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &auto4, None)
        .await
        .unwrap();
    assert_eq!(
        run.status,
        RunStatus::Failed,
        "an unenforced constraint (rate_limit) fails closed"
    );

    // 5. A `cost_limit` grant is now ENFORCED (the agent loop caps per-run spend,
    //    §7/§19), so it no longer fails closed: a non-LLM action under it has zero
    //    spend → the cap is trivially satisfied → the run succeeds and commits.
    let cost_capped = store
        .grants()
        .upsert(
            ws.id,
            "cost-capped",
            &[Capability::new(Action::Write, Resource::domain("notes"))],
            &Constraints {
                cost_limit: Some(1.0),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let auto5 = store
        .automations()
        .create(
            ws.id,
            &catalerum_store::NewAutomation {
                name: "cost-capped".into(),
                enabled: true,
                triggers: vec![json!({ "kind": "webhook", "path": "/run" })],
                condition: None,
                actions: vec![json!({
                    "kind": "create_note", "title": "COST-OK-MARKER", "markdown": "x"
                })],
                spec: None,
                grant_id: Some(cost_capped.id),
            },
        )
        .await
        .unwrap();
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &auto5, None)
        .await
        .unwrap();
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "a cost_limit grant is enforced, not failed-closed → a zero-spend action commits"
    );
    let notes = store
        .notes()
        .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
        .await
        .unwrap();
    assert!(
        notes.iter().any(|n| n.title == "COST-OK-MARKER"),
        "the cost-capped run must actually commit its side effect (note created)"
    );
}

#[test]
fn role_rank_orders_by_privilege() {
    assert!(role_rank(Role::Owner) < role_rank(Role::Admin));
    assert!(role_rank(Role::Admin) < role_rank(Role::Member));
    assert!(role_rank(Role::Member) < role_rank(Role::Viewer));
}

#[tokio::test]
async fn workspace_owner_authority_acts_as_the_workspace_owner() {
    let Some(url) = db_url() else {
        eprintln!("skipping workspace-owner-authority test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("ownerauth", &format!("ownerauth-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    // Two members: a Member (created first) and the Owner (created later).
    // The runner must pick the *Owner* as the acting identity regardless of
    // creation order — proving rank, not just "first member".
    let member = store
        .users()
        .create(
            &format!("m-{}@t.test", uuid::Uuid::new_v4()),
            "Member",
            None,
        )
        .await
        .expect("member user");
    store
        .memberships()
        .upsert(ws.id, member.id, Role::Member)
        .await
        .expect("member");
    let owner = store
        .users()
        .create(&format!("o-{}@t.test", uuid::Uuid::new_v4()), "Owner", None)
        .await
        .expect("owner user");
    store
        .memberships()
        .upsert(ws.id, owner.id, Role::Owner)
        .await
        .expect("owner");

    // embed/graph off → only Postgres needed for create_note.
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());

    // An automation that creates a note — no explicit identity/authority is
    // pinned, so the runner resolves the workspace owner + base-Member caps.
    let auto = store
        .automations()
        .create(
            ws.id,
            &automation(
                "owner-bot",
                vec![json!({
                    "kind": "create_note", "title": "by the owner", "markdown": "hi"
                })],
            ),
        )
        .await
        .expect("automation");
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &auto, None)
        .await
        .unwrap();
    assert_eq!(
        run.status,
        RunStatus::Succeeded,
        "Member caps cover create_note"
    );
    let notes = store
        .notes()
        .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
        .await
        .unwrap();
    let note = notes
        .iter()
        .find(|n| n.title == "by the owner")
        .expect("note created");
    assert_eq!(
        note.author,
        Author::User { id: owner.id },
        "the automation acted as the resolved workspace owner, not the lower-ranked member"
    );
}

#[tokio::test]
async fn workspace_owner_authority_fails_when_the_workspace_has_no_members() {
    let Some(url) = db_url() else {
        eprintln!("skipping no-members test: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    // A workspace with no memberships → no identity to act as → the action
    // fails cleanly (a `Failed` run), never an unauthenticated side effect.
    let ws = store
        .workspaces()
        .create("nomembers", &format!("nomembers-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let registry = build_registry(
        &store,
        None,
        NoteIngest::new(store.clone(), false, false),
        None,
        None,
        None,
        Vec::new(),
        None,
        None,
    );
    let runner = ToolActionRunner::workspace_owner_authority(registry, store.clone());
    let auto = store
        .automations()
        .create(
            ws.id,
            &automation(
                "orphan-bot",
                vec![json!({
                    "kind": "create_note", "title": "should not exist"
                })],
            ),
        )
        .await
        .expect("automation");
    let run = execute(&store, &runner, &FailCodeRunner, ws.id, &auto, None)
        .await
        .unwrap();
    assert_eq!(run.status, RunStatus::Failed);
    let steps = store
        .automation_runs()
        .list_steps(ws.id, run.id)
        .await
        .unwrap();
    assert!(steps[0].error.as_deref().unwrap().contains("no members"));
    assert!(store
        .notes()
        .list_by_workspace(ws.id, catalerum_store::DEFAULT_NOTE_LIMIT)
        .await
        .unwrap()
        .is_empty());
}

#[test]
fn agent_request_defaults_model_and_seeds_system_plus_user_nudge() {
    use catalerum_core::model::MessageRole;
    // No model / tools pinned, no trigger → default model, the given system,
    // the bare nudge as the user turn, all tools advertised (None).
    let (req, allowed) = agent_request(
        &LlmAgent::default(),
        "echo",
        DEFAULT_AGENT_SYSTEM.to_string(),
        None,
    );
    assert_eq!(req.model, "echo");
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.messages[0].role, MessageRole::System);
    assert_eq!(req.messages[0].content, DEFAULT_AGENT_SYSTEM);
    assert_eq!(req.messages[1].role, MessageRole::User);
    assert_eq!(req.messages[1].content, AGENT_TRIGGER_PROMPT);
    assert!(
        allowed.is_none(),
        "empty tools → advertise the whole registry"
    );
}

#[test]
fn agent_request_honors_pinned_model_and_tool_subset() {
    let agent = LlmAgent {
        system: None,
        model: Some("gpt-x".into()),
        tools: vec!["create_note".into(), "recall".into()],
        skills: vec![],
        output: None,
        reasoning_effort: None,
    };
    let (req, allowed) = agent_request(&agent, "echo", "be terse".to_string(), None);
    assert_eq!(
        req.model, "gpt-x",
        "the action's model wins over the default"
    );
    assert_eq!(req.messages[0].content, "be terse");
    assert_eq!(
        allowed.as_deref(),
        Some(&["create_note".to_string(), "recall".to_string()][..])
    );
}

#[test]
fn trigger_prompt_describes_the_firing_event() {
    // No trigger → the bare nudge.
    assert_eq!(trigger_prompt(None), AGENT_TRIGGER_PROMPT);
    // A trigger → its JSON is embedded so the agent knows what fired it.
    let p = trigger_prompt(Some(&json!({ "kind": "webhook", "path": "/deploy-done" })));
    assert!(p.contains("webhook") && p.contains("/deploy-done"));
    assert!(p.contains(AGENT_TRIGGER_PROMPT));
}

#[test]
fn agent_outcome_empty_output_is_a_failed_step() {
    use catalerum_core::model::{StepStatus, ToolCall};
    use catalerum_llm::ToolInvocation;

    // Neither text nor tool calls → Failed (surfaces a no-op / broken-LLM turn,
    // rather than silently succeeding with empty output).
    let empty = AgentOutcome {
        content: "  ".into(),
        iterations: 1,
        ..Default::default()
    };
    let out = agent_outcome_to_action(&empty, None);
    assert_eq!(out.status, StepStatus::Failed);
    assert!(out.error.as_deref().unwrap().contains("no output"));

    // Non-empty text → Succeeded, carrying the content; no `data` without it.
    let answered = AgentOutcome {
        content: "here you go".into(),
        iterations: 1,
        ..Default::default()
    };
    let out = agent_outcome_to_action(&answered, None);
    assert_eq!(out.status, StepStatus::Succeeded);
    assert_eq!(
        out.output.as_ref().unwrap()["content"],
        json!("here you go")
    );
    assert!(out.output.as_ref().unwrap().get("data").is_none());

    // A JSON-steered agent attaches its parsed reply as `data` for downstream use.
    let out = agent_outcome_to_action(&answered, Some(json!({ "yes": 0.8, "no": 0.2 })));
    assert_eq!(
        out.output.as_ref().unwrap()["data"],
        json!({ "yes": 0.8, "no": 0.2 })
    );

    // No text but a tool ran → Succeeded (the agent did real work via tools).
    let acted = AgentOutcome {
        content: String::new(),
        iterations: 2,
        tool_invocations: vec![ToolInvocation {
            call: ToolCall {
                id: "1".into(),
                name: "create_note".into(),
                arguments: "{}".into(),
            },
            result: "{}".into(),
            is_error: false,
            duration_ms: 0,
            media: Vec::new(),
        }],
        ..Default::default()
    };
    let out = agent_outcome_to_action(&acted, None);
    assert_eq!(out.status, StepStatus::Succeeded);
    assert_eq!(out.output.as_ref().unwrap()["tool_calls"], json!(1));

    // Cost + truncation status surface into the step output (for the run-detail
    // cost chip / "budget reached" badge, §19). A clean run omits the flags;
    // a cost-capped run carries `cost_usd` + `cost_capped`.
    let clean = agent_outcome_to_action(&answered, None);
    let clean_out = clean.output.as_ref().unwrap();
    assert!(clean_out.get("cost_usd").is_none());
    assert!(clean_out.get("cost_capped").is_none());
    assert!(clean_out.get("iteration_capped").is_none());
    assert!(clean_out.get("tool_loop_capped").is_none());

    let capped = AgentOutcome {
        content: "partial".into(),
        iterations: 3,
        usage: Some(catalerum_core::stream::Usage {
            cost_usd: Some(1.25),
            ..Default::default()
        }),
        hit_cost_limit: true,
        ..Default::default()
    };
    let out = agent_outcome_to_action(&capped, None);
    let o = out.output.as_ref().unwrap();
    assert_eq!(o["cost_usd"], json!(1.25));
    assert_eq!(o["cost_capped"], json!(true));
    assert!(o.get("iteration_capped").is_none());

    let loop_capped = AgentOutcome {
        content: "partial".into(),
        iterations: 3,
        hit_tool_loop_cap: true,
        ..Default::default()
    };
    let out = agent_outcome_to_action(&loop_capped, None);
    assert_eq!(
        out.output.as_ref().unwrap()["tool_loop_capped"],
        json!(true)
    );
}

#[test]
fn extract_json_handles_bare_and_fenced_replies() {
    // Bare JSON.
    assert_eq!(
        extract_json(r#"{"yes":0.7,"no":0.3}"#),
        Some(json!({ "yes": 0.7, "no": 0.3 }))
    );
    // A ```json fenced block (the common LLM habit).
    assert_eq!(
        extract_json("```json\n{\"a\": 1}\n```"),
        Some(json!({ "a": 1 }))
    );
    // A bare ``` fence + surrounding whitespace.
    assert_eq!(extract_json("  ```\n[1, 2]\n```  "), Some(json!([1, 2])));
    // Prose around JSON doesn't parse → None (the agent step keeps raw content).
    assert_eq!(extract_json("Sure! Here you go: {\"a\":1}"), None);
    assert_eq!(extract_json("not json at all"), None);
}

#[test]
fn system_with_skills_appends_runbooks_or_leaves_base_unchanged() {
    // No skills → base unchanged.
    assert_eq!(system_with_skills("base prompt", &[]), "base prompt");
    // Skills → base + each runbook, in order.
    let s = system_with_skills("base prompt", &["do X first".into(), "then do Y".into()]);
    assert!(s.starts_with("base prompt"));
    assert!(s.contains("# Skills"));
    let x = s.find("do X first").expect("skill 1 present");
    let y = s.find("then do Y").expect("skill 2 present");
    assert!(x < y, "skills are appended in order");
}

#[tokio::test]
async fn llm_agent_without_a_client_fails_cleanly() {
    // A runner with no `with_llm` reports a clear Failed outcome for an
    // LlmAgent action (rather than panicking or silently succeeding).
    let runner = ToolActionRunner::new(ToolRegistry::default());
    let action = Action {
        kind: ActionKind::LlmAgent,
        params: serde_json::Map::new(),
    };
    let outcome = runner.run(WorkspaceId::new(), &action, None, None).await;
    assert_eq!(outcome.status, StepStatus::Failed);
    assert!(outcome
        .error
        .as_deref()
        .unwrap()
        .contains("needs an LLM client"));
}

#[tokio::test]
async fn summarize_without_a_client_fails_cleanly() {
    // Summarize, like LlmAgent, needs the client — without one it reports a
    // clear Failed outcome rather than panicking or silently succeeding.
    let runner = ToolActionRunner::new(ToolRegistry::default());
    let action = Action {
        kind: ActionKind::Summarize,
        params: serde_json::Map::from_iter([("input".to_string(), json!("some text"))]),
    };
    let outcome = runner.run(WorkspaceId::new(), &action, None, None).await;
    assert_eq!(outcome.status, StepStatus::Failed);
    assert!(outcome
        .error
        .as_deref()
        .unwrap()
        .contains("needs an LLM client"));
}

#[test]
fn summarize_request_seeds_input_instructions_and_model() {
    // Explicit input + instructions + max_words + pinned model.
    let params = serde_json::Map::from_iter([
        ("input".to_string(), json!("  the text to condense  ")),
        ("instructions".to_string(), json!("Focus on action items.")),
        ("max_words".to_string(), json!(50)),
        ("model".to_string(), json!("some/model")),
    ]);
    let req = summarize_request(&params, None, "default/model").unwrap();
    assert_eq!(req.model, "some/model");
    assert_eq!(req.messages.len(), 2);
    let system = &req.messages[0].content;
    assert!(system.contains("Focus on action items."));
    assert!(system.contains("under 50 words"));
    assert_eq!(req.messages[1].content, "the text to condense");

    // No input → the firing trigger event is summarized (graph-context shape:
    // the event lives under `trigger`, mirroring `firing_event`).
    let trigger = json!({ "trigger": { "kind": "webhook", "path": "/x" }, "inputs": {} });
    let req =
        summarize_request(&serde_json::Map::new(), Some(&trigger), "default/model").unwrap();
    assert_eq!(req.model, "default/model");
    assert!(req.messages[1].content.contains("\"kind\": \"webhook\""));
    // …and the wrapper's `inputs` is NOT what gets summarized.
    assert!(!req.messages[1].content.contains("inputs"));

    // A whole-value template resolves to non-string JSON → pretty-printed.
    let params = serde_json::Map::from_iter([("input".to_string(), json!({ "a": [1, 2] }))]);
    let req = summarize_request(&params, None, "default/model").unwrap();
    assert!(req.messages[1].content.contains("\"a\""));
}

#[test]
fn summarize_request_rejects_nothing_to_summarize_and_truncates() {
    // No input and no trigger → a clear error.
    let err = summarize_request(&serde_json::Map::new(), None, "m").unwrap_err();
    assert!(err.contains("no `input`"), "got: {err}");
    // An input that template-resolved to empty → a clear error (a silent
    // empty summary would hide a broken {{ path }}).
    let params = serde_json::Map::from_iter([("input".to_string(), json!("   "))]);
    let err = summarize_request(&params, None, "m").unwrap_err();
    assert!(err.contains("empty"), "got: {err}");
    // Oversized input truncates on a char boundary with an explicit marker.
    let big = "é".repeat(MAX_SUMMARIZE_INPUT_BYTES); // 2 bytes/char → over cap
    let params = serde_json::Map::from_iter([("input".to_string(), json!(big))]);
    let req = summarize_request(&params, None, "m").unwrap();
    let user = &req.messages[1].content;
    assert!(user.ends_with("[input truncated]"));
    assert!(user.len() <= MAX_SUMMARIZE_INPUT_BYTES + 32);
}

#[tokio::test]
async fn run_skill_without_a_client_fails_cleanly() {
    // RunSkill, like LlmAgent, runs the §7 loop — without an LLM client it reports
    // a clear Failed outcome rather than panicking or silently succeeding.
    let runner = ToolActionRunner::new(ToolRegistry::default());
    let action = Action {
        kind: ActionKind::RunSkill,
        params: serde_json::Map::from_iter([("skill".to_string(), json!("triage-inbox"))]),
    };
    let outcome = runner.run(WorkspaceId::new(), &action, None, None).await;
    assert_eq!(outcome.status, StepStatus::Failed);
    assert!(outcome
        .error
        .as_deref()
        .unwrap()
        .contains("needs an LLM client"));
}

#[test]
fn channel_from_trigger_extracts_channel_message_only() {
    assert_eq!(
        channel_from_trigger(Some(
            &json!({ "kind": "channel_message", "channel": "ops", "text": "hi" })
        )),
        Some("ops".to_string())
    );
    assert_eq!(
        channel_from_trigger(Some(&json!({ "kind": "webhook", "path": "/x" }))),
        None
    );
    assert_eq!(channel_from_trigger(None), None);
    // A channel message missing its channel name → nothing to reply to.
    assert_eq!(
        channel_from_trigger(Some(&json!({ "kind": "channel_message" }))),
        None
    );
}

#[test]
fn channel_helpers_unwrap_graph_wrapped_context() {
    // In a graph run the executor wraps the context as `{ "trigger": <event>,
    // "inputs": … }`; the channel helpers must still find the channel/text, else
    // the SOUL §25 in-channel reply silently no-ops for graph automations (works
    // in the linear path, which passes the raw event).
    let graph = json!({
        "trigger": { "kind": "channel_message", "channel": "tg:42", "text": "hi there" },
        "inputs": { "some_node": { "data": 1 } }
    });
    assert_eq!(
        channel_from_trigger(Some(&graph)),
        Some("tg:42".to_string())
    );
    assert_eq!(
        channel_text_from_trigger(Some(&graph)),
        Some("hi there".to_string())
    );

    // The linear shape (the event itself) still works.
    let linear = json!({ "kind": "channel_message", "channel": "ops", "text": "yo" });
    assert_eq!(channel_from_trigger(Some(&linear)), Some("ops".to_string()));
    assert_eq!(
        channel_text_from_trigger(Some(&linear)),
        Some("yo".to_string())
    );

    // A graph run of a non-channel trigger → no channel reply.
    let graph_webhook = json!({ "trigger": { "kind": "webhook", "path": "/x" }, "inputs": {} });
    assert_eq!(channel_from_trigger(Some(&graph_webhook)), None);

    // firing_event unwraps the graph shape and passes the linear one through.
    assert_eq!(
        firing_event(Some(&graph))
            .and_then(|e| e.get("kind"))
            .and_then(Value::as_str),
        Some("channel_message")
    );
    assert_eq!(
        firing_event(Some(&linear))
            .and_then(|e| e.get("kind"))
            .and_then(Value::as_str),
        Some("channel_message")
    );
}

/// A [`catalerum_channels::Channel`] that records the text it's sent — proves an
/// agent's reply is delivered to the triggering channel (no LLM needed).
struct RecordingChannel(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

#[async_trait]
impl catalerum_channels::Channel for RecordingChannel {
    fn kind(&self) -> &str {
        "test"
    }
    async fn send(
        &self,
        msg: &catalerum_channels::OutMessage,
    ) -> catalerum_channels::Result<()> {
        self.0.lock().unwrap().push(msg.text.clone());
        Ok(())
    }
}

#[tokio::test]
async fn channel_triggered_agent_reply_is_delivered_to_the_channel() {
    use std::sync::{Arc, Mutex};
    let sent = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut channels = std::collections::HashMap::new();
    channels.insert(
        "ops".to_string(),
        Arc::new(RecordingChannel(sent.clone())) as Arc<dyn catalerum_channels::Channel>,
    );
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(crate::tools::NotifyTool::new(channels)));
    let runner = ToolActionRunner::new(registry);
    // base Member holds `channel:write`, so the reply dispatch is authorized.
    let ctx = ToolContext {
        workspace_id: Some(WorkspaceId::new()),
        capabilities: Some(catalerum_iam::base_capabilities(Role::Member)),
        ..Default::default()
    };

    // A channel-message trigger → the (trimmed) reply lands on that channel.
    let trigger = json!({ "kind": "channel_message", "channel": "ops", "text": "hi" });
    runner
        .deliver_channel_reply(&ctx, Some(&trigger), "  hello from the bot  ")
        .await;
    assert_eq!(
        sent.lock().unwrap().as_slice(),
        &["hello from the bot".to_string()]
    );

    // A non-channel trigger and an empty reply both deliver nothing.
    runner
        .deliver_channel_reply(&ctx, Some(&json!({ "kind": "webhook", "path": "/x" })), "x")
        .await;
    runner
        .deliver_channel_reply(&ctx, Some(&trigger), "   ")
        .await;
    assert_eq!(
        sent.lock().unwrap().len(),
        1,
        "only the channel-triggered, non-empty reply was sent"
    );
}
