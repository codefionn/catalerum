//! Integration test: weekly plans (SOUL §8) — plan + entry CRUD, duplicate,
//! the plan ⇄ event link table (upsert, slot re-pointing, applications summary,
//! FK lifecycle: event delete cascades the link, entry delete orphans it, plan
//! delete keeps the event), and cross-workspace isolation (§18).
//!
//! Same DB gating as the other store tests: set `CATALERUM_TEST_DATABASE_URL`
//! (or `DATABASE_URL`) to run it; otherwise it skips and passes offline.

use catalerum_core::model::WeeklyPlanEntry;
use catalerum_store::{PlanEntryInput, PlanInput, Store, StoreError, UpsertEvent};
use chrono::{NaiveDate, TimeZone, Utc};

fn test_db_url() -> Option<String> {
    std::env::var("CATALERUM_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

fn entry<'a>(weekday: i32, start: i32, end: i32, summary: &'a str) -> PlanEntryInput<'a> {
    PlanEntryInput {
        weekday,
        start_minute: start,
        end_minute: end,
        all_day: false,
        summary,
        location: None,
        body: None,
        labels: &[],
        calendar_id: None,
    }
}

#[tokio::test]
async fn weekly_plan_crud_and_event_links() {
    let Some(url) = test_db_url() else {
        eprintln!(
            "skipping weekly_plan_crud_and_event_links: \
             set CATALERUM_TEST_DATABASE_URL or DATABASE_URL to run it"
        );
        return;
    };
    let store = Store::connect(&url).await.expect("connect+migrate");
    let ws = store
        .workspaces()
        .create("plans", &format!("plans-{}", uuid::Uuid::new_v4()))
        .await
        .unwrap();
    let other = store
        .workspaces()
        .create("plans-b", &format!("plans-b-{}", uuid::Uuid::new_v4()))
        .await
        .unwrap();
    let repo = store.weekly_plans();

    // --- plan + entries ---------------------------------------------------
    let plan = repo
        .create(
            ws.id,
            &PlanInput {
                name: "Normal week",
                description: Some("default rhythm"),
                calendar_id: None,
                timezone: "Europe/Berlin",
            },
        )
        .await
        .unwrap();
    assert!(plan.entries.is_empty());
    assert_eq!(plan.timezone, "Europe/Berlin");

    let labels = vec!["sport".to_string()];
    let gym = repo
        .add_entry(
            ws.id,
            plan.id,
            &PlanEntryInput {
                labels: &labels,
                ..entry(2, 18 * 60, 19 * 60 + 30, "Gym")
            },
        )
        .await
        .unwrap();
    let standup = repo
        .add_entry(ws.id, plan.id, &entry(0, 9 * 60, 9 * 60 + 15, "Standup"))
        .await
        .unwrap();
    assert_eq!(gym.labels, labels);

    let got = repo.get(ws.id, plan.id).await.unwrap();
    // Ordered by weekday then start: Monday standup before Wednesday gym.
    let order: Vec<&str> = got.entries.iter().map(|e| e.summary.as_str()).collect();
    assert_eq!(order, ["Standup", "Gym"]);

    let gym = repo
        .update_entry(ws.id, plan.id, gym.id, &entry(3, 18 * 60, 20 * 60, "Gym (long)"))
        .await
        .unwrap();
    assert_eq!((gym.weekday, gym.end_minute), (3, 20 * 60));
    assert!(gym.labels.is_empty(), "update replaces labels");

    // A slot outside the CHECK ranges is refused by the table even if a caller
    // skipped validation.
    assert!(WeeklyPlanEntry::validate_slot(7, 0, 10, false).is_err());
    assert!(repo
        .add_entry(ws.id, plan.id, &entry(7, 0, 10, "bad"))
        .await
        .is_err());

    // Entries can't be addressed through a foreign plan / workspace.
    let other_plan = repo
        .create(
            other.id,
            &PlanInput {
                name: "Theirs",
                description: None,
                calendar_id: None,
                timezone: "UTC",
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        repo.update_entry(ws.id, other_plan.id, gym.id, &entry(1, 0, 10, "x"))
            .await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        repo.add_entry(ws.id, other_plan.id, &entry(1, 0, 10, "x")).await,
        Err(StoreError::NotFound)
    ));
    assert!(matches!(
        repo.get(other.id, plan.id).await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(repo.list_by_workspace(ws.id).await.unwrap().len(), 1);

    // Header update keeps entries.
    let renamed = repo
        .update(
            ws.id,
            plan.id,
            &PlanInput {
                name: "Regular week",
                description: None,
                calendar_id: None,
                timezone: "UTC",
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.name, "Regular week");
    assert_eq!(renamed.entries.len(), 2);

    // Duplicate copies entries under fresh ids.
    let copy = repo.duplicate(ws.id, plan.id, "Next week").await.unwrap();
    assert_ne!(copy.id, plan.id);
    assert_eq!(copy.entries.len(), 2);
    assert!(copy.entries.iter().all(|e| e.plan_id == copy.id));
    assert!(copy
        .entries
        .iter()
        .all(|e| e.id != gym.id && e.id != standup.id));

    // --- links --------------------------------------------------------------
    let cal = store
        .calendars()
        .upsert_local(ws.id, "default", "Calendar")
        .await
        .unwrap();
    let week = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
    let mk_event = |uid: &'static str| {
        let store = store.clone();
        let ws_id = ws.id;
        async move {
            store
                .events()
                .create(&UpsertEvent::new(
                    ws_id,
                    cal.id,
                    uid,
                    "x",
                    Utc.with_ymd_and_hms(2026, 10, 5, 9, 0, 0).unwrap(),
                    Utc.with_ymd_and_hms(2026, 10, 5, 10, 0, 0).unwrap(),
                ))
                .await
                .unwrap()
        }
    };
    let e1 = mk_event("e1").await;
    let e2 = mk_event("e2").await;
    let e3 = mk_event("e3").await;

    repo.link_event(ws.id, plan.id, standup.id, e1.id, week)
        .await
        .unwrap();
    repo.link_event(ws.id, plan.id, gym.id, e2.id, week)
        .await
        .unwrap();
    // Idempotent re-link of the same event.
    repo.link_event(ws.id, plan.id, gym.id, e2.id, week)
        .await
        .unwrap();
    assert_eq!(repo.links_for_week(ws.id, plan.id, week).await.unwrap().len(), 2);

    // Re-pointing the (gym, week) slot at a new event replaces the old link.
    repo.link_event(ws.id, plan.id, gym.id, e3.id, week)
        .await
        .unwrap();
    let links = repo.links_for_week(ws.id, plan.id, week).await.unwrap();
    assert_eq!(links.len(), 2);
    assert!(links.iter().any(|l| l.event_id == e3.id));
    assert!(links.iter().all(|l| l.event_id != e2.id));
    assert!(repo.link_for_event(ws.id, e2.id).await.unwrap().is_none());
    assert_eq!(
        repo.link_for_event(ws.id, e1.id).await.unwrap().unwrap().entry_id,
        Some(standup.id)
    );
    assert!(repo.link_for_event(other.id, e1.id).await.unwrap().is_none());

    let apps = repo.applications(ws.id, plan.id).await.unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!((apps[0].week_start, apps[0].event_count), (week, 2));
    assert!(repo.applications(other.id, plan.id).await.unwrap().is_empty());
    assert_eq!(repo.list_event_links(ws.id, 100).await.unwrap().len(), 2);

    // Deleting an event elsewhere drops its link (FK cascade).
    store.events().delete(ws.id, e3.id).await.unwrap();
    assert_eq!(repo.links_for_week(ws.id, plan.id, week).await.unwrap().len(), 1);

    // Deleting an entry orphans its link (entry_id NULL) for re-apply cleanup.
    repo.delete_entry(ws.id, plan.id, standup.id).await.unwrap();
    let orphan = repo.link_for_event(ws.id, e1.id).await.unwrap().unwrap();
    assert_eq!(orphan.entry_id, None);
    assert!(matches!(
        repo.delete_entry(ws.id, plan.id, standup.id).await,
        Err(StoreError::NotFound)
    ));

    // Deleting the plan drops links but keeps the events.
    repo.delete(ws.id, plan.id).await.unwrap();
    assert!(repo.link_for_event(ws.id, e1.id).await.unwrap().is_none());
    assert!(store.events().get(ws.id, e1.id).await.is_ok());
    assert!(matches!(
        repo.delete(ws.id, plan.id).await,
        Err(StoreError::NotFound)
    ));
}
