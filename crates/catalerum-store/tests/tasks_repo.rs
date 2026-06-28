//! Integration test: the `BoardRepo` + `TaskRepo` contract (SOUL §24, §6.1/§18).
//! Board/column creation, task create → next → move → complete flow, the
//! column-belongs-to-board tenancy guard, and cross-workspace isolation.
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::{Author, TaskStatus};
use catalerum_core::UserId;
use catalerum_store::{Store, StoreError};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

#[tokio::test]
async fn board_and_task_flow_with_tenancy_guards() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping board_and_task_flow_with_tenancy_guards: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };

    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("tasks", &format!("tasks-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("tasks-b", &format!("tasks-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    // Default board has the four standard columns, in order.
    let board = store
        .boards()
        .create(ws.id, "Ops", &[])
        .await
        .expect("board");
    assert_eq!(
        board
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Backlog", "To-do", "Doing", "Done"]
    );
    assert_eq!(board.columns[0].order, 0);
    let backlog = board.columns[0].id;
    let doing = board.columns[2].id;

    // Two tasks in Backlog get dense ordinals.
    let t1 = store
        .tasks()
        .create(
            ws.id,
            board.id,
            backlog,
            "deploy",
            "do the deploy",
            Some(Author::User { id: UserId::new() }),
        )
        .await
        .expect("t1");
    let t2 = store
        .tasks()
        .create(ws.id, board.id, backlog, "write docs", "", None)
        .await
        .expect("t2");
    assert_eq!(t1.order, 0);
    assert_eq!(t2.order, 1);
    assert_eq!(t1.status, TaskStatus::Open);
    assert!(t1.assignee.is_some());

    // next_in_column pulls the lowest-ordinal non-done task.
    let next = store
        .tasks()
        .next_in_column(ws.id, backlog)
        .await
        .unwrap()
        .expect("a next task");
    assert_eq!(next.id, t1.id);

    // Move t1 to Doing → it leaves Backlog; next is now t2.
    let moved = store
        .tasks()
        .move_to_column(ws.id, t1.id, doing, None)
        .await
        .expect("move");
    assert_eq!(moved.column_id, doing);
    let next2 = store
        .tasks()
        .next_in_column(ws.id, backlog)
        .await
        .unwrap()
        .expect("next after move");
    assert_eq!(next2.id, t2.id);

    // Complete t1 (status done) → it is no longer "next" in Doing.
    store
        .tasks()
        .set_status(ws.id, t1.id, TaskStatus::Done)
        .await
        .expect("complete");
    assert!(store
        .tasks()
        .next_in_column(ws.id, doing)
        .await
        .unwrap()
        .is_none());

    // Tenancy guard: can't create a task in another workspace's board/column.
    assert!(matches!(
        store
            .tasks()
            .create(other.id, board.id, backlog, "intruder", "", None)
            .await,
        Err(StoreError::NotFound)
    ));
    // ...nor with a column that isn't in the named board.
    let board2 = store.boards().create(ws.id, "Other", &["A"]).await.unwrap();
    assert!(matches!(
        store
            .tasks()
            .create(ws.id, board.id, board2.columns[0].id, "x", "", None)
            .await,
        Err(StoreError::NotFound)
    ));

    // Cross-workspace isolation: `other` can't see `ws`'s board.
    assert!(matches!(
        store.boards().get(other.id, board.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(store
        .boards()
        .list_by_workspace(other.id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn positioned_moves_and_column_management() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping positioned_moves_and_column_management: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("taskpos", &format!("taskpos-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let board = store
        .boards()
        .create(ws.id, "Sprint", &["A", "B"])
        .await
        .expect("board");
    let (a, b) = (board.columns[0].id, board.columns[1].id);

    let mut ids = Vec::new();
    for title in ["one", "two", "three"] {
        ids.push(
            store
                .tasks()
                .create(ws.id, board.id, a, title, "", None)
                .await
                .expect("task")
                .id,
        );
    }
    let titles_in = |col| {
        let store = store.clone();
        async move {
            store
                .tasks()
                .list_by_column(ws.id, col)
                .await
                .unwrap()
                .into_iter()
                .map(|t| t.title)
                .collect::<Vec<_>>()
        }
    };

    // Same-column reorder: "three" to the top.
    store
        .tasks()
        .move_to_column(ws.id, ids[2], a, Some(0))
        .await
        .expect("reorder");
    assert_eq!(titles_in(a).await, vec!["three", "one", "two"]);

    // Cross-column move into the middle of B (after seeding one task there).
    store
        .tasks()
        .move_to_column(ws.id, ids[0], b, None)
        .await
        .expect("seed B");
    store
        .tasks()
        .move_to_column(ws.id, ids[1], b, Some(0))
        .await
        .expect("move to top of B");
    assert_eq!(titles_in(b).await, vec!["two", "one"]);
    // An out-of-range position clamps to the end.
    store
        .tasks()
        .move_to_column(ws.id, ids[2], b, Some(99))
        .await
        .expect("clamped");
    assert_eq!(titles_in(b).await, vec!["two", "one", "three"]);
    // Ordinals were renumbered densely.
    let orders: Vec<i32> = store
        .tasks()
        .list_by_column(ws.id, b)
        .await
        .unwrap()
        .iter()
        .map(|t| t.order)
        .collect();
    assert_eq!(orders, vec![0, 1, 2]);

    // Column management: add, rename, and delete-empty; a non-empty or last
    // column refuses deletion.
    let with_qa = store
        .boards()
        .add_column(ws.id, board.id, "QA")
        .await
        .expect("add column");
    assert_eq!(
        with_qa
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["A", "B", "QA"]
    );
    let qa = with_qa.columns[2].id;
    let renamed = store
        .boards()
        .rename_column(ws.id, qa, "Review")
        .await
        .expect("rename column");
    assert_eq!(renamed.columns[2].name, "Review");
    assert!(matches!(
        store.boards().delete_column(ws.id, b).await,
        Err(StoreError::Invalid(_))
    ));
    let after = store
        .boards()
        .delete_column(ws.id, qa)
        .await
        .expect("delete empty column");
    assert_eq!(after.columns.len(), 2);

    // Tenancy: a foreign workspace can't touch this board's columns.
    let other = store
        .workspaces()
        .create("taskpos-b", &format!("taskpos-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");
    assert!(matches!(
        store.boards().add_column(other.id, board.id, "X").await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.boards().rename_column(other.id, a, "X").await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        store.boards().delete_column(other.id, a).await,
        Err(StoreError::NotFound)
    ));
}

#[tokio::test]
async fn search_in_workspace_matches_title_or_body_and_is_scoped() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping search_in_workspace_matches_title_or_body_and_is_scoped: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("tsearch", &format!("tsearch-{}", uuid::Uuid::new_v4()))
        .await
        .expect("ws");
    let other = store
        .workspaces()
        .create("tsearch-b", &format!("tsearch-b-{}", uuid::Uuid::new_v4()))
        .await
        .expect("other ws");

    let board = store
        .boards()
        .create(ws.id, "Sprint", &[])
        .await
        .expect("board");
    let col = board.columns[0].id;
    // A title match, a body match, and a non-match — all in `ws`.
    store
        .tasks()
        .create(ws.id, board.id, col, "Migrate the database", "", None)
        .await
        .expect("title-match");
    store
        .tasks()
        .create(
            ws.id,
            board.id,
            col,
            "Unrelated",
            "details about the MIGRATION plan",
            None,
        )
        .await
        .expect("body-match");
    store
        .tasks()
        .create(ws.id, board.id, col, "Lunch", "nothing relevant", None)
        .await
        .expect("miss");
    // A matching task in ANOTHER workspace — must never leak.
    let oboard = store
        .boards()
        .create(other.id, "B", &["A"])
        .await
        .expect("ob");
    store
        .tasks()
        .create(
            other.id,
            oboard.id,
            oboard.columns[0].id,
            "secret migration",
            "",
            None,
        )
        .await
        .expect("foreign");

    // Case-insensitive substring over title OR body, scoped to `ws`.
    let hits = store
        .tasks()
        .search_in_workspace(ws.id, "migrat", 50)
        .await
        .expect("search");
    let titles: std::collections::HashSet<String> = hits.iter().map(|t| t.title.clone()).collect();
    assert_eq!(
        hits.len(),
        2,
        "the title-match + the body-match (not Lunch, not the other ws)"
    );
    assert!(
        titles.contains("Migrate the database"),
        "matched in the title"
    );
    assert!(titles.contains("Unrelated"), "matched in the body");
    assert!(
        !titles.contains("secret migration"),
        "a task in another workspace never leaks"
    );

    // Blank query → nothing (never "match everything").
    assert!(store
        .tasks()
        .search_in_workspace(ws.id, "   ", 50)
        .await
        .expect("blank")
        .is_empty());
}
