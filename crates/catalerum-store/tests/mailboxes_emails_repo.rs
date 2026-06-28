//! Integration test: the `MailboxRepo` + `EmailRepo` contract (SOUL §28,
//! §6.1/§18). Mailbox upsert idempotency by `(connection_id, external_id)`, email
//! upsert idempotency by `(mailbox_id, uid)` (incl. JSONB address/flag
//! round-trips), key + listing lookups, and cross-workspace isolation (§18).
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{Attachment, ConnectionKind, Email, EmailAddress};
use catalerum_core::{EmailId, MailboxId, WorkspaceId};
use catalerum_store::{Store, StoreError};
use chrono::DateTime;

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn sample_email(
    workspace_id: WorkspaceId,
    mailbox_id: MailboxId,
    uid: &str,
    subject: &str,
    flags: Vec<String>,
) -> Email {
    Email {
        id: EmailId::new(),
        workspace_id,
        mailbox_id,
        uid: uid.to_string(),
        message_id: Some(format!("<{uid}@example.com>")),
        from: Some(EmailAddress {
            name: Some("Ada Lovelace".into()),
            address: "ada@example.com".into(),
        }),
        to: vec![
            EmailAddress::new("charles@example.com"),
            EmailAddress {
                name: Some("Friend".into()),
                address: "friend@example.org".into(),
            },
        ],
        cc: vec![EmailAddress::new("cc@example.com")],
        subject: subject.to_string(),
        received_at: Some(chrono::Utc::now()),
        body_text: Some("hello there".into()),
        body_html: None,
        has_attachments: false,
        flags,
        labels: vec![],
        raw_ref: None,
        attachments: vec![],
        raw: None,
    }
}

#[tokio::test]
async fn mailbox_and_email_upsert_idempotent_and_isolated() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping mailbox_and_email_upsert_idempotent_and_isolated: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("mail", &format!("mail-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("mail-b", &format!("mail-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("email connection");

    // Mailbox upsert is idempotent on (connection_id, external_id).
    let m1 = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/var/mail/inbox", "INBOX", true)
        .await
        .expect("mailbox upsert");
    let m2 = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/var/mail/inbox", "Inbox (renamed)", true)
        .await
        .expect("mailbox re-upsert");
    assert_eq!(m1.id, m2.id, "mailbox upsert preserves id");
    assert_eq!(m2.name, "Inbox (renamed)", "name refreshed on conflict");
    assert_eq!(
        store
            .mailboxes()
            .list_by_workspace(ws.id)
            .await
            .unwrap()
            .len(),
        1,
        "no duplicate mailbox"
    );

    // Email upsert by (mailbox_id, uid): JSONB addresses/flags round-trip.
    let e1 = store
        .emails()
        .upsert_by_uid(&sample_email(
            ws.id,
            m1.id,
            "uid-1",
            "Hello",
            vec!["seen".into()],
        ))
        .await
        .expect("email upsert");
    assert_eq!(e1.subject, "Hello");
    assert_eq!(e1.from.as_ref().unwrap().address, "ada@example.com");
    assert_eq!(
        e1.from.as_ref().unwrap().name.as_deref(),
        Some("Ada Lovelace")
    );
    assert_eq!(e1.to.len(), 2);
    assert_eq!(e1.to[1].address, "friend@example.org");
    assert_eq!(e1.cc.len(), 1);
    assert_eq!(e1.flags, vec!["seen".to_string()]);
    assert_eq!(e1.message_id.as_deref(), Some("<uid-1@example.com>"));

    // Re-upsert the SAME uid with a changed subject + flag → same id, refreshed.
    let e1b = store
        .emails()
        .upsert_by_uid(&sample_email(
            ws.id,
            m1.id,
            "uid-1",
            "Hello (edited)",
            vec!["seen".into(), "flagged".into()],
        ))
        .await
        .expect("email re-upsert");
    assert_eq!(e1b.id, e1.id, "email upsert preserves id on (mailbox, uid)");
    assert_eq!(e1b.subject, "Hello (edited)");
    assert_eq!(e1b.flags, vec!["seen".to_string(), "flagged".to_string()]);

    // A distinct uid is its own row.
    store
        .emails()
        .upsert_by_uid(&sample_email(ws.id, m1.id, "uid-2", "Second", vec![]))
        .await
        .expect("second email");

    // Lookups.
    let by_uid = store
        .emails()
        .get_by_uid(ws.id, m1.id, "uid-1")
        .await
        .expect("get_by_uid");
    assert_eq!(by_uid.subject, "Hello (edited)");
    let by_id = store.emails().get(ws.id, e1.id).await.expect("get by id");
    assert_eq!(by_id.uid, "uid-1");

    let all = store.emails().list_by_workspace(ws.id, 50).await.unwrap();
    assert_eq!(all.len(), 2, "two distinct emails");
    let in_box = store
        .emails()
        .list_by_mailbox(ws.id, m1.id, 50)
        .await
        .unwrap();
    assert_eq!(in_box.len(), 2);

    // §18: another workspace sees none of it.
    assert!(store
        .mailboxes()
        .list_by_workspace(other.id)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .emails()
        .list_by_workspace(other.id, 50)
        .await
        .unwrap()
        .is_empty());
    assert!(matches!(
        store.emails().get_by_uid(other.id, m1.id, "uid-1").await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn cross_folder_same_message_id_stays_n_rows_and_groups() {
    // SOUL §29 cross-folder dedup, resolved: the same RFC 5322 message appearing in
    // two folders is TWO rows (one per mailbox — deletion/flags are per-folder), not
    // one, and `list_by_message_id` collapses them by their shared Message-ID so a
    // caller can treat the N folder-copies as one logical email.
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping cross_folder_same_message_id_stays_n_rows_and_groups: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("maildedup", &format!("maildedup-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("conn");
    // Two folders (mailboxes) on the one connection: INBOX and Archive.
    let inbox = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/m/inbox", "INBOX", true)
        .await
        .expect("inbox");
    let archive = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/m/archive", "Archive", true)
        .await
        .expect("archive");
    assert_ne!(
        inbox.id, archive.id,
        "distinct folders are distinct mailboxes"
    );

    // The SAME logical message in both folders: one Message-ID, but each folder's
    // copy is keyed by its own `(mailbox_id, uid)`. `sample_email` derives the
    // Message-ID from the uid, so give both copies a uid that yields the same id.
    const MID: &str = "<shared-thread@example.com>";
    let mut in_inbox = sample_email(ws.id, inbox.id, "inbox-uid", "Shared", vec![]);
    in_inbox.message_id = Some(MID.to_string());
    let mut in_archive = sample_email(
        ws.id,
        archive.id,
        "archive-uid",
        "Shared",
        vec!["seen".into()],
    );
    in_archive.message_id = Some(MID.to_string());
    let a = store
        .emails()
        .upsert_by_uid(&in_inbox)
        .await
        .expect("inbox copy");
    let b = store
        .emails()
        .upsert_by_uid(&in_archive)
        .await
        .expect("archive copy");

    // N rows stay: two distinct rows, distinct ids + mailboxes, shared Message-ID.
    assert_ne!(a.id, b.id, "cross-folder copies are distinct rows");
    assert_ne!(a.mailbox_id, b.mailbox_id);
    assert_eq!(
        store
            .emails()
            .list_by_workspace(ws.id, 50)
            .await
            .unwrap()
            .len(),
        2
    );

    // Grouping collapses them by Message-ID (the §29 dedup surface).
    let grouped = store
        .emails()
        .list_by_message_id(ws.id, MID)
        .await
        .expect("group by message_id");
    assert_eq!(
        grouped.len(),
        2,
        "both folder-copies group under one Message-ID"
    );
    let mut ids: Vec<_> = grouped.iter().map(|e| e.id).collect();
    ids.sort();
    let mut want = vec![a.id, b.id];
    want.sort();
    assert_eq!(ids, want);

    // An unknown Message-ID groups nothing.
    assert!(store
        .emails()
        .list_by_message_id(ws.id, "<nobody@example.com>")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn folders_by_message_id_groups_a_page_scoped_to_its_ids() {
    // The page-scoped group query behind the inbox listing's cross-folder dedup
    // annotation (SOUL §29): given the DISTINCT non-null Message-IDs of a listed page, it
    // returns — per id, in ONE query — the set of folder (mailbox) names that message is
    // filed under workspace-wide, so a row can be annotated "also in N other folders".
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping folders_by_message_id_groups_a_page_scoped_to_its_ids: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("mailgrp", &format!("mailgrp-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("conn");
    let inbox = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/m/inbox", "INBOX", true)
        .await
        .expect("inbox");
    let archive = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/m/archive", "Archive", true)
        .await
        .expect("archive");
    let sent = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/m/sent", "Sent", true)
        .await
        .expect("sent");

    // A cross-filed message: same Message-ID in INBOX, Archive, and Sent (3 folders).
    const SHARED: &str = "<shared-thread@example.com>";
    for (mb, uid, flags) in [
        (inbox.id, "shared-inbox", vec![]),
        (archive.id, "shared-archive", vec!["seen".to_string()]),
        (sent.id, "shared-sent", vec!["seen".to_string()]),
    ] {
        let mut e = sample_email(ws.id, mb, uid, "Shared", flags);
        e.message_id = Some(SHARED.to_string());
        store.emails().upsert_by_uid(&e).await.expect("shared copy");
    }
    // A single-filed message: only in INBOX.
    const SOLO: &str = "<solo@example.com>";
    let mut solo = sample_email(ws.id, inbox.id, "solo-uid", "Solo", vec![]);
    solo.message_id = Some(SOLO.to_string());
    store.emails().upsert_by_uid(&solo).await.expect("solo");

    let emails = store.emails();

    // Group over BOTH ids (a "page"): shared spans 3 folders, solo spans 1.
    let grouped = emails
        .folders_by_message_id(ws.id, &[SHARED.to_string(), SOLO.to_string()])
        .await
        .expect("group");
    let mut shared_folders = grouped.get(SHARED).cloned().expect("shared present");
    shared_folders.sort();
    assert_eq!(
        shared_folders,
        vec![
            "Archive".to_string(),
            "INBOX".to_string(),
            "Sent".to_string()
        ],
        "the shared message groups its three distinct folders"
    );
    assert_eq!(
        grouped.get(SOLO).map(Vec::as_slice),
        Some(&["INBOX".to_string()][..]),
        "a single-filed message maps to its one folder"
    );

    // Page-scoped: an id NOT in the requested set is never returned even though it exists.
    let only_solo = emails
        .folders_by_message_id(ws.id, &[SOLO.to_string()])
        .await
        .expect("only solo");
    assert!(only_solo.contains_key(SOLO));
    assert!(
        !only_solo.contains_key(SHARED),
        "the group query is scoped to the ids it's handed"
    );

    // An unknown id yields no entry; empty input issues no query and returns empty.
    assert!(emails
        .folders_by_message_id(ws.id, &["<nobody@example.com>".to_string()])
        .await
        .expect("unknown")
        .is_empty());
    assert!(emails
        .folders_by_message_id(ws.id, &[])
        .await
        .expect("empty")
        .is_empty());

    // §18: another workspace's group query sees none of it.
    let other = store
        .workspaces()
        .create("mailgrp-b", &format!("mailgrp-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    assert!(emails
        .folders_by_message_id(other.id, &[SHARED.to_string(), SOLO.to_string()])
        .await
        .expect("other ws group")
        .is_empty());
}

#[tokio::test]
async fn set_labels_replaces_and_survives_a_resync() {
    // The `LabelEmail` automation action path (SOUL §11/§28): a classifier verdict
    // is a full replace of the email's free-text labels, kept separate from `flags`
    // (provider tokens) and NOT clobbered by a later provider re-upsert.
    let Some(url) = test_db_url() else {
        eprintln!("skipping set_labels_replaces_and_survives_a_resync: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("maillbl", &format!("maillbl-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("conn");
    let mb = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/m", "INBOX", true)
        .await
        .expect("mailbox");

    let e = store
        .emails()
        .upsert_by_uid(&sample_email(
            ws.id,
            mb.id,
            "uid-x",
            "Receipt",
            vec!["seen".into()],
        ))
        .await
        .expect("email");
    assert!(e.labels.is_empty(), "a freshly written email has no labels");

    // Set a verdict → full replace; flags are untouched.
    let labeled = store
        .emails()
        .set_labels(ws.id, e.id, &["receipt".to_string(), "urgent".to_string()])
        .await
        .expect("set_labels");
    assert_eq!(
        labeled.labels,
        vec!["receipt".to_string(), "urgent".to_string()]
    );
    assert_eq!(
        labeled.flags,
        vec!["seen".to_string()],
        "labels don't touch flags"
    );

    // A provider re-sync of the SAME uid (no labels in the upsert) must NOT clobber
    // the verdict — labels are written only by set_labels (like raw_ref).
    let resynced = store
        .emails()
        .upsert_by_uid(&sample_email(
            ws.id,
            mb.id,
            "uid-x",
            "Receipt (refetched)",
            vec!["seen".into(), "answered".into()],
        ))
        .await
        .expect("re-upsert");
    assert_eq!(resynced.id, e.id);
    assert_eq!(
        resynced.labels,
        vec!["receipt".to_string(), "urgent".to_string()],
        "a re-sync preserves the LabelEmail verdict"
    );

    // A second verdict fully replaces the first (idempotent overwrite).
    let relabeled = store
        .emails()
        .set_labels(ws.id, e.id, &["archived".to_string()])
        .await
        .expect("re-label");
    assert_eq!(relabeled.labels, vec!["archived".to_string()]);

    // set_labels on a missing email surfaces NotFound (a verdict for unwritten mail).
    assert!(matches!(
        store
            .emails()
            .set_labels(ws.id, EmailId::new(), &["x".to_string()])
            .await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn list_untagged_filters_in_sql_and_is_workspace_scoped() {
    // The backlog feed for a scheduled classify sweep (SOUL §11/§28): only
    // label-less mail comes back, filtered server-side so old untagged messages
    // stay reachable however many labelled ones are newer; §18 scoping holds.
    let Some(url) = test_db_url() else {
        eprintln!("skipping list_untagged_filters_in_sql_and_is_workspace_scoped: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("mailut", &format!("mailut-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("conn");
    let mb = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/m", "INBOX", true)
        .await
        .expect("mailbox");

    let labeled = store
        .emails()
        .upsert_by_uid(&sample_email(ws.id, mb.id, "ut-1", "Labelled", vec![]))
        .await
        .expect("email 1");
    store
        .emails()
        .set_labels(ws.id, labeled.id, &["work".to_string()])
        .await
        .expect("set_labels");
    store
        .emails()
        .upsert_by_uid(&sample_email(ws.id, mb.id, "ut-2", "Bare A", vec![]))
        .await
        .expect("email 2");
    store
        .emails()
        .upsert_by_uid(&sample_email(ws.id, mb.id, "ut-3", "Bare B", vec![]))
        .await
        .expect("email 3");

    let untagged = store
        .emails()
        .list_untagged_by_workspace(ws.id, 50)
        .await
        .expect("list untagged");
    let mut uids: Vec<&str> = untagged.iter().map(|e| e.uid.as_str()).collect();
    uids.sort_unstable();
    assert_eq!(uids, vec!["ut-2", "ut-3"], "only label-less mail, ut-1 out");
    assert!(untagged.iter().all(|e| e.labels.is_empty()));

    // The limit bounds the page (the newest untagged wins the tie on equal
    // received_at via the id tie-break — just assert the count here).
    let page = store
        .emails()
        .list_untagged_by_workspace(ws.id, 1)
        .await
        .expect("limited page");
    assert_eq!(page.len(), 1);

    // §18: a foreign workspace sees none of it.
    let other = store
        .workspaces()
        .create("mailut2", &format!("mailut2-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    assert!(store
        .emails()
        .list_untagged_by_workspace(other.id, 50)
        .await
        .expect("foreign list")
        .is_empty());
}

#[tokio::test]
async fn set_seen_toggles_and_unread_counts_group_per_mailbox() {
    // The mark-read path (SOUL §28): `set_seen` flips ONLY the local `seen` flag
    // (case-insensitively, never duplicating it, preserving other tokens), and
    // `unread_counts_by_mailbox` groups the sidebar badge numbers in one query.
    let Some(url) = test_db_url() else {
        eprintln!("skipping set_seen_toggles_and_unread_counts_group_per_mailbox: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("mailseen", &format!("mailseen-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("conn");
    let inbox = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/m/inbox", "INBOX", true)
        .await
        .expect("inbox");
    let archive = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/m/archive", "Archive", true)
        .await
        .expect("archive");

    // INBOX: two unread (one with a case-variant flag set), one read.
    let u1 = store
        .emails()
        .upsert_by_uid(&sample_email(ws.id, inbox.id, "u1", "One", vec![]))
        .await
        .expect("u1");
    store
        .emails()
        .upsert_by_uid(&sample_email(
            ws.id,
            inbox.id,
            "u2",
            "Two",
            vec!["flagged".into()],
        ))
        .await
        .expect("u2");
    store
        .emails()
        .upsert_by_uid(&sample_email(
            ws.id,
            inbox.id,
            "r1",
            "Read",
            vec!["Seen".into()],
        ))
        .await
        .expect("r1");
    // Archive: one unread.
    store
        .emails()
        .upsert_by_uid(&sample_email(ws.id, archive.id, "a1", "Old", vec![]))
        .await
        .expect("a1");

    let counts = store
        .emails()
        .unread_counts_by_mailbox(ws.id)
        .await
        .expect("counts");
    assert_eq!(counts.get(&inbox.id), Some(&2), "INBOX has two unread");
    assert_eq!(counts.get(&archive.id), Some(&1), "Archive has one unread");

    // Mark read: appends the normalized token, preserves other flags.
    let read = store
        .emails()
        .set_seen(ws.id, u1.id, true)
        .await
        .expect("mark read");
    assert_eq!(read.flags, vec!["seen".to_string()]);
    // Idempotent: marking read again never duplicates the token.
    let again = store
        .emails()
        .set_seen(ws.id, u1.id, true)
        .await
        .expect("mark read again");
    assert_eq!(again.flags, vec!["seen".to_string()]);
    let counts = store
        .emails()
        .unread_counts_by_mailbox(ws.id)
        .await
        .expect("counts after read");
    assert_eq!(counts.get(&inbox.id), Some(&1), "one fewer unread");

    // Mark unread strips the flag case-insensitively ("Seen" too), keeping others.
    let r1 = store
        .emails()
        .get_by_uid(ws.id, inbox.id, "r1")
        .await
        .expect("r1 row");
    let unread = store
        .emails()
        .set_seen(ws.id, r1.id, false)
        .await
        .expect("mark unread");
    assert!(unread.flags.is_empty(), "case-variant Seen stripped");
    let u2 = store
        .emails()
        .get_by_uid(ws.id, inbox.id, "u2")
        .await
        .expect("u2 row");
    let u2_read = store
        .emails()
        .set_seen(ws.id, u2.id, true)
        .await
        .expect("u2 read");
    assert_eq!(
        u2_read.flags,
        vec!["flagged".to_string(), "seen".to_string()],
        "other provider tokens survive the toggle"
    );

    // A missing email surfaces NotFound; a foreign workspace sees no counts.
    assert!(matches!(
        store.emails().set_seen(ws.id, EmailId::new(), true).await,
        Err(StoreError::NotFound)
    ));
    let other = store
        .workspaces()
        .create(
            "mailseen-b",
            &format!("mailseen-b-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("other ws");
    assert!(store
        .emails()
        .unread_counts_by_mailbox(other.id)
        .await
        .expect("other counts")
        .is_empty());
}

#[tokio::test]
async fn set_attachments_records_refs_and_survives_a_resync() {
    // The archival path (SOUL §9/§28/§29): `WriteEmail`-triggered archival writes the
    // attachments to the files store and links them here as references — a full
    // replace kept separate from the provider upsert, NOT clobbered by a re-sync
    // (mirrors `set_raw_ref` / `set_labels`).
    let Some(url) = test_db_url() else {
        eprintln!("skipping set_attachments_records_refs_and_survives_a_resync: set CATALERUM_TEST_DATABASE_URL or DATABASE_URL");
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("mailatt", &format!("mailatt-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("conn");
    let mb = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/m", "INBOX", true)
        .await
        .expect("mailbox");

    let e = store
        .emails()
        .upsert_by_uid(&sample_email(
            ws.id,
            mb.id,
            "uid-a",
            "With attachment",
            vec!["seen".into()],
        ))
        .await
        .expect("email");
    assert!(
        e.attachments.is_empty(),
        "a freshly written email has no attachment refs"
    );

    let refs = vec![Attachment {
        url: "/storage/objects/emails/mb/uid-a/attachments/0-invoice.pdf".into(),
        filename: Some("invoice.pdf".into()),
        content_type: Some("application/pdf".into()),
        size: Some(1234),
    }];
    store
        .emails()
        .set_attachments(ws.id, e.id, &refs)
        .await
        .expect("set_attachments");
    let got = store.emails().get(ws.id, e.id).await.expect("get");
    assert_eq!(got.attachments, refs, "attachment refs round-trip");

    // A provider re-sync of the SAME uid must NOT clobber the archived refs.
    let resynced = store
        .emails()
        .upsert_by_uid(&sample_email(
            ws.id,
            mb.id,
            "uid-a",
            "Refetched",
            vec!["seen".into()],
        ))
        .await
        .expect("re-upsert");
    assert_eq!(resynced.id, e.id);
    assert_eq!(
        resynced.attachments, refs,
        "a re-sync preserves archived attachment refs"
    );
}

#[tokio::test]
async fn get_many_batches_and_is_workspace_scoped() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping get_many_batches_and_is_workspace_scoped: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("mailgm", &format!("mailgm-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("mailgm-b", &format!("mailgm-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("conn");
    let mbox = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/inbox", "INBOX", false)
        .await
        .expect("mailbox");
    let e1 = store
        .emails()
        .upsert_by_uid(&sample_email(ws.id, mbox.id, "uid-1", "One", vec![]))
        .await
        .expect("e1");
    let e2 = store
        .emails()
        .upsert_by_uid(&sample_email(ws.id, mbox.id, "uid-2", "Two", vec![]))
        .await
        .expect("e2");
    store
        .emails()
        .upsert_by_uid(&sample_email(ws.id, mbox.id, "uid-3", "Three", vec![]))
        .await
        .expect("e3");

    // A foreign-workspace email (its own connection + mailbox) — must never leak.
    let other_conn = store
        .connections()
        .create(other.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("other conn");
    let other_mbox = store
        .mailboxes()
        .upsert(other.id, other_conn.id, "/inbox", "INBOX", false)
        .await
        .expect("other mailbox");
    let foreign = store
        .emails()
        .upsert_by_uid(&sample_email(
            other.id,
            other_mbox.id,
            "uid-x",
            "Foreign",
            vec![],
        ))
        .await
        .expect("foreign");

    // Batch: request two in-workspace ids, a foreign id, and a bogus id in one call.
    // Only the two in `ws` come back (foreign + bogus silently omitted, §18).
    let bogus = EmailId::new();
    let got = store
        .emails()
        .get_many(ws.id, &[e1.id, e2.id, foreign.id, bogus])
        .await
        .expect("get_many");
    let ids: std::collections::HashSet<EmailId> = got.iter().map(|e| e.id).collect();
    assert_eq!(
        got.len(),
        2,
        "only the two in-workspace ids return (foreign + bogus omitted)"
    );
    assert!(ids.contains(&e1.id) && ids.contains(&e2.id));
    assert!(
        !ids.contains(&foreign.id),
        "a foreign-workspace id never leaks through get_many"
    );

    // Empty input → empty result, no query issued.
    assert!(store
        .emails()
        .get_many(ws.id, &[])
        .await
        .expect("empty")
        .is_empty());
}

#[tokio::test]
async fn mailbox_get_many_batches_and_is_workspace_scoped() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping mailbox_get_many_batches_and_is_workspace_scoped: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("mbgm", &format!("mbgm-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("mbgm-b", &format!("mbgm-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("conn");
    let m1 = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/inbox", "INBOX", false)
        .await
        .expect("m1");
    let m2 = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/sent", "Sent", false)
        .await
        .expect("m2");
    // A foreign-workspace mailbox — must never leak through get_many.
    let other_conn = store
        .connections()
        .create(other.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("other conn");
    let foreign = store
        .mailboxes()
        .upsert(other.id, other_conn.id, "/inbox", "INBOX", false)
        .await
        .expect("foreign");

    let bogus = MailboxId::new();
    let got = store
        .mailboxes()
        .get_many(ws.id, &[m1.id, m2.id, foreign.id, bogus])
        .await
        .expect("get_many");
    let ids: std::collections::HashSet<MailboxId> = got.iter().map(|m| m.id).collect();
    assert_eq!(
        got.len(),
        2,
        "only the two in-workspace mailboxes (foreign + bogus omitted)"
    );
    assert!(ids.contains(&m1.id) && ids.contains(&m2.id));
    assert!(
        !ids.contains(&foreign.id),
        "a foreign-workspace mailbox never leaks"
    );

    // Empty input → empty, no query.
    assert!(store
        .mailboxes()
        .get_many(ws.id, &[])
        .await
        .expect("empty")
        .is_empty());
}

#[tokio::test]
async fn search_in_workspace_filters_in_sql_before_the_limit() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping search_in_workspace_filters_in_sql_before_the_limit: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create(
            "mailsearch",
            &format!("mailsearch-{}", uuid::Uuid::new_v4()),
        )
        .await
        .expect("ws");
    let conn = store
        .connections()
        .create(ws.id, ConnectionKind::Email, "maildir", None, None)
        .await
        .expect("conn");
    let mbox = store
        .mailboxes()
        .upsert(ws.id, conn.id, "/inbox", "INBOX", false)
        .await
        .expect("mbox");

    // The OLDEST email is the only match for "quarterly" / sender "ada-old", and is
    // the only unread one; then five NEWER, read, non-matching emails. A recency
    // scan-then-filter with a small limit would never see the old match — the SQL
    // filter must (the bug this fixes).
    let mut old = sample_email(
        ws.id,
        mbox.id,
        "uid-old",
        "Quarterly report attached",
        vec![],
    );
    old.received_at = Some(DateTime::from_timestamp(1_000, 0).unwrap());
    old.from = Some(EmailAddress {
        name: Some("Ada Old".into()),
        address: "ada-old@example.com".into(),
    });
    store.emails().upsert_by_uid(&old).await.expect("old");
    for i in 0..5 {
        let mut e = sample_email(
            ws.id,
            mbox.id,
            &format!("uid-new-{i}"),
            "Lunch plans",
            vec!["seen".into()],
        );
        e.received_at = Some(DateTime::from_timestamp(9_000 + i as i64, 0).unwrap());
        store.emails().upsert_by_uid(&e).await.expect("new");
    }

    let emails = store.emails();
    // Content search, limit 2: a plain recency-list of 2 is the newest (no match);
    // the SQL filter finds the old "Quarterly" mail regardless.
    let by_content = emails
        .search_in_workspace(ws.id, None, Some("quarterly"), None, None, 2)
        .await
        .expect("content");
    assert_eq!(by_content.len(), 1);
    assert_eq!(by_content[0].uid, "uid-old");

    // Sender substring (address or name), still found past the recency window.
    let by_sender = emails
        .search_in_workspace(ws.id, None, None, Some("ada-old"), None, 2)
        .await
        .expect("sender");
    assert_eq!(by_sender.len(), 1);
    assert_eq!(by_sender[0].uid, "uid-old");

    // Unread mirrors `is_unread` (no case-insensitive `seen` flag): only the old one.
    let unread = emails
        .search_in_workspace(ws.id, None, None, None, Some(true), 50)
        .await
        .expect("unread");
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].uid, "uid-old");
    let read = emails
        .search_in_workspace(ws.id, None, None, None, Some(false), 50)
        .await
        .expect("read");
    assert_eq!(read.len(), 5, "the five seen emails");

    // No predicates = plain recent list, bounded, newest-first.
    let recent = emails
        .search_in_workspace(ws.id, None, None, None, None, 3)
        .await
        .expect("recent");
    assert_eq!(recent.len(), 3);
    assert_eq!(recent[0].uid, "uid-new-4", "newest first");

    // Mailbox scoping: a foreign mailbox id yields nothing.
    let scoped = emails
        .search_in_workspace(ws.id, Some(MailboxId::new()), None, None, None, 50)
        .await
        .expect("scoped");
    assert!(scoped.is_empty());
}
