//! catalerum-skills — markdown-defined skill registry (SOUL §23).
//!
//! A **skill** is a reusable, named capability bundle: a markdown `instructions`
//! runbook, an optional restricted tool set, and optional `code` (run via the
//! Executor §20). Skills are stored per workspace ([`catalerum_store::SkillRepo`])
//! and invoked by the LLM via the `use_skill` tool, capability-gated
//! (`skill:use@<name>`, §19); a skill can never exceed the caller's grant.
//!
//! This crate owns the **first-party fixtures** (`summarize`, `triage-inbox`,
//! `weekly-review`, `share-file`) and the idempotent [`seed_first_party`] helper. Persistence
//! lives in `catalerum-store`; richer invocation (restricted-tool enforcement
//! during the agent loop, and running a skill's `code` via the Executor) layers
//! on later.

#![forbid(unsafe_code)]

use catalerum_core::model::Skill;
use catalerum_core::WorkspaceId;
use catalerum_store::{NewSkill, Result, Store};

/// The first-party skills that ship as fixtures (SOUL §23). Pure-instructions
/// runbooks (no `code`) for now, each restricted to a small, sensible tool set
/// (a subset of the §7 registry). Authored markdown-first, like runbooks.
#[must_use]
pub fn first_party_skills() -> Vec<NewSkill> {
    vec![
        NewSkill {
            name: "summarize".to_string(),
            description: "Summarize content (a note, search results, or a conversation) concisely."
                .to_string(),
            instructions_md: "\
# Summarize

Produce a concise, faithful summary.

1. Gather the source material — if given an id, read it (`read_note`); if given a \
   topic, pull the most relevant content with `search_semantic`.
2. Capture the key points, decisions, and any action items — drop filler.
3. Return a short summary: a one-line gist, then 3–6 bullet points. Do not invent \
   facts; if the material is thin, say so."
                .to_string(),
            tools: vec![
                "read_note".into(),
                "search_semantic".into(),
                "list_notes".into(),
            ],
            code: None,
            advertised: true,
        },
        NewSkill {
            name: "triage-inbox".to_string(),
            description: "Turn incoming notes/items into actionable Kanban tasks.".to_string(),
            instructions_md: "\
# Triage inbox

Convert unprocessed items into tasks on the board.

1. List recent notes (`list_notes`) and find anything that implies an action.
2. For each actionable item, create a task (`kanban_create_task`) with a clear title and \
   a short body summarizing what to do and why.
3. Skip anything purely informational. Report what you created and what you skipped."
                .to_string(),
            tools: vec![
                "list_notes".into(),
                "search_semantic".into(),
                "kanban_create_task".into(),
            ],
            code: None,
            advertised: true,
        },
        NewSkill {
            name: "weekly-review".to_string(),
            description: "Produce a weekly review from recent notes, memories, and events."
                .to_string(),
            instructions_md: "\
# Weekly review

Synthesize the week into a short review note.

1. Pull recent notes (`query_structured` → `recent_notes`) and upcoming events \
   (`upcoming_events`).
2. Recall relevant standing context with `recall` and `search_semantic`.
3. Draft a review covering: what happened, what's pending, what's next week. Save \
   it as a note (`create_note`) titled `Weekly review — <date>`."
                .to_string(),
            tools: vec![
                "query_structured".into(),
                "upcoming_events".into(),
                "search_semantic".into(),
                "recall".into(),
                "create_note".into(),
            ],
            code: None,
            advertised: true,
        },
        NewSkill {
            name: "share-file".to_string(),
            description: "Give the user a clickable download link for a stored file or directory."
                .to_string(),
            instructions_md: "\
# Share a file

Hand the user a link they can click to download a file (or a whole folder).

1. Identify the file to share by its store-relative `key`. If it isn't in a files \
   store yet (e.g. it's still in a terminal workdir), put it there first — \
   `copy_object` relocates it into a store the link can reach.
2. Mint the link with `download_link` (pass the `key`; add `store` if it isn't your \
   default). For a whole directory, pass the folder's key ending in `/` — it's \
   delivered as a `.tar.gz`. Set `ttl_secs` if the default hour is wrong.
3. Give the user the returned `url` verbatim and mention when it expires. The link \
   needs no login, so don't wrap or shorten it."
                .to_string(),
            tools: vec!["download_link".into(), "copy_object".into()],
            code: None,
            advertised: true,
        },
    ]
}

/// Idempotently seed the [`first_party_skills`] into `workspace_id` (SOUL §23).
/// Upserts by name, so re-running refreshes definitions without duplicating.
/// Returns the seeded skills.
pub async fn seed_first_party(store: &Store, workspace_id: WorkspaceId) -> Result<Vec<Skill>> {
    let mut seeded = Vec::new();
    for spec in first_party_skills() {
        seeded.push(store.skills().upsert_by_name(workspace_id, &spec).await?);
    }
    Ok(seeded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn first_party_skills_are_well_formed_and_uniquely_named() {
        let skills = first_party_skills();
        assert!(skills.len() >= 3);
        let names: HashSet<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names.len(), skills.len(), "skill names are unique");
        assert!(names.contains("summarize"));
        assert!(names.contains("triage-inbox"));
        assert!(names.contains("weekly-review"));
        assert!(names.contains("share-file"));
        for s in &skills {
            assert!(!s.name.is_empty());
            assert!(!s.description.is_empty());
            assert!(!s.instructions_md.is_empty(), "{} has instructions", s.name);
            assert!(!s.tools.is_empty(), "{} restricts to a tool set", s.name);
            assert!(
                s.code.is_none(),
                "first-party skills are runbooks (no code) for now"
            );
        }
    }

    #[test]
    fn skills_grant_every_tool_their_runbook_invokes() {
        // A skill's `tools` allow-list must include every registry tool its
        // `instructions_md` tells the model to call (a `` `tool_name` `` backtick
        // reference), or — once §23 restricted-tool enforcement lands — the skill
        // couldn't perform what it documents. Guards against the tools list drifting
        // out of sync with the runbook (as `weekly-review` did with `upcoming_events`).
        // The set of real registry tools the fixtures reference:
        const TOOLS: &[&str] = &[
            "read_note",
            "search_semantic",
            "list_notes",
            "kanban_create_task",
            "query_structured",
            "upcoming_events",
            "recall",
            "create_note",
            "download_link",
            "copy_object",
        ];
        for s in first_party_skills() {
            for tool in TOOLS {
                let backtick = format!("`{tool}`");
                if s.instructions_md.contains(&backtick) {
                    assert!(
                        s.tools.iter().any(|t| t == tool),
                        "skill `{}` invokes `{tool}` in its runbook but doesn't grant it: {:?}",
                        s.name,
                        s.tools,
                    );
                }
            }
        }
    }
}
