//! The Tasks panel (SOUL §24, §12 — Kanban board).
//!
//! A board view over the Kanban REST surface (`/boards`, `/boards/{id}/tasks`,
//! `/boards/{id}/columns`, `/columns/{id}`, `/tasks/{id}/move`,
//! `/tasks/{id}/status`) — the HTTP face of the existing `BoardRepo`/`TaskRepo`.
//! Every call carries the dev session token and is workspace-scoped +
//! `tasks:read`/`write`-gated server-side (SOUL §18/§19).
//!
//! A board selector + create-board + a client-side card filter sit on top;
//! below, the selected board's columns render side-by-side, each holding its
//! task cards. Cards move by **drag-and-drop**: dropping on a column appends,
//! dropping **onto a card inserts above it** (a positioned `/move`, so a
//! same-column drop reorders). Clicking a card opens a **detail modal** — the
//! rendered markdown body, status + column selects (the accessible move
//! fallback), edit mode (title input + [`MarkdownField`]), and delete (with a
//! confirm). Columns are managed inline: rename/delete from the column header,
//! append via the trailing "+ Add column" stub. Board delete confirms first
//! (it cascades all columns + tasks).

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::dialogs::{use_dialogs, ConfirmSpec};
use super::markdown::markdown_html;
use super::md_editor::MarkdownField;
use crate::api::{
    AddColumn, Board, CreateBoard, CreateTask, EditTask, MoveTask, RenameBoard, RenameColumn,
    SetTaskStatus, Task,
};
use crate::auth;
use crate::components::icons::{Icon, MdIcon};
use crate::rest;

/// Where a dragged card would land: the hovered column, plus the card it would
/// be inserted **above** (`None` = the column's end).
type DropTarget = Option<(String, Option<String>)>;

/// The Tasks (Kanban) panel component.
#[component]
pub fn TasksPanel() -> impl IntoView {
    // The shared confirm dialog (replaces native delete confirms).
    let dialogs = use_dialogs();
    let boards = RwSignal::new(Vec::<Board>::new());
    let selected_board = RwSignal::new(Option::<String>::None);
    let tasks = RwSignal::new(Vec::<Task>::new());
    let loading = RwSignal::new(true);
    let error = RwSignal::new(Option::<String>::None);
    let busy = RwSignal::new(false);

    // Drag-and-drop state: the task id being dragged + the current drop target
    // (for the column highlight and the insert-above indicator on a card).
    let dragging = RwSignal::new(Option::<String>::None);
    let drag_over = RwSignal::new(DropTarget::None);

    // New-board name, the client-side card filter, and the per-column add-task draft.
    let new_board_name = RwSignal::new(String::new());
    let filter = RwSignal::new(String::new());
    let add_col = RwSignal::new(Option::<String>::None);
    let add_title = RwSignal::new(String::new());

    // Detail modal: the open task's id, whether its editor is active, and the
    // editor's title/body drafts (seeded when Edit is clicked).
    let detail = RwSignal::new(Option::<String>::None);
    let detail_edit = RwSignal::new(false);
    let edit_title = RwSignal::new(String::new());
    let edit_body = RwSignal::new(String::new());

    // Inline board-rename draft: whether the rename form is open + its name field,
    // seeded from the selected board when the ✎ in the header is clicked.
    let renaming_board = RwSignal::new(false);
    let board_rename_name = RwSignal::new(String::new());

    // Column management drafts: the trailing add-column stub + the per-column
    // inline rename (the column id being renamed; None = no rename open).
    let add_column_open = RwSignal::new(false);
    let new_col_name = RwSignal::new(String::new());
    let renaming_col = RwSignal::new(Option::<String>::None);
    let col_rename_name = RwSignal::new(String::new());

    // Fetch the selected board's tasks.
    let load_tasks = move || {
        let Some(board_id) = selected_board.get_untracked() else {
            tasks.set(Vec::new());
            return;
        };
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_board_tasks(token.as_deref(), &board_id).await {
                Ok(list) => {
                    tasks.set(list);
                    error.set(None);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Fetch boards; optionally auto-select the first, then load its tasks.
    let load_boards = move |auto_select: bool| {
        loading.set(true);
        error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::list_boards(token.as_deref()).await {
                Ok(list) => {
                    if auto_select && selected_board.get_untracked().is_none() {
                        if let Some(first) = list.first() {
                            selected_board.set(Some(first.id.clone()));
                        }
                    }
                    boards.set(list);
                    error.set(None);
                    load_tasks();
                }
                Err(e) => {
                    boards.set(Vec::new());
                    error.set(Some(e.to_string()));
                }
            }
            loading.set(false);
        });
    };

    load_boards(true);

    // Create a board with the default column set.
    let create_board = move || {
        if busy.get_untracked() {
            return;
        }
        let name = new_board_name.get_untracked().trim().to_string();
        if name.is_empty() {
            error.set(Some("Give the board a name.".to_string()));
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let token = auth::resolve_token();
            match rest::create_board(
                token.as_deref(),
                &CreateBoard {
                    name,
                    columns: Vec::new(),
                },
            )
            .await
            {
                Ok(board) => {
                    new_board_name.set(String::new());
                    selected_board.set(Some(board.id.clone()));
                    busy.set(false);
                    load_boards(false);
                }
                Err(e) => {
                    busy.set(false);
                    error.set(Some(e.to_string()));
                }
            }
        });
    };

    // Submit the inline add-task form for the open column.
    let submit_add = move || {
        if busy.get_untracked() {
            return;
        }
        let (Some(board_id), Some(column_id)) =
            (selected_board.get_untracked(), add_col.get_untracked())
        else {
            return;
        };
        let title = add_title.get_untracked().trim().to_string();
        if title.is_empty() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result = rest::create_task(
                token.as_deref(),
                &board_id,
                &CreateTask {
                    column_id,
                    title,
                    body_md: String::new(),
                },
            )
            .await;
            busy.set(false);
            match result {
                Ok(_) => {
                    add_title.set(String::new());
                    load_tasks();
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Move a task to `to_column`; `position` is its final 0-based index there
    // (None = the end) — a same-column move with a position is a reorder.
    let do_move = move |task_id: String, to_column: String, position: Option<i32>| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result = rest::move_task(
                token.as_deref(),
                &task_id,
                &MoveTask {
                    column_id: to_column,
                    position,
                },
            )
            .await;
            busy.set(false);
            match result {
                Ok(_) => load_tasks(),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Change a task's status.
    let do_status = move |task_id: String, status: String| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result =
                rest::set_task_status(token.as_deref(), &task_id, &SetTaskStatus { status }).await;
            busy.set(false);
            match result {
                Ok(_) => load_tasks(),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Delete a task (card) from its board. The confirm lives at the call site
    // (the modal), which knows the task's title.
    let do_delete = move |task_id: String| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result = rest::delete_task(token.as_deref(), &task_id).await;
            busy.set(false);
            match result {
                Ok(()) => load_tasks(),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Save the modal's open editor (PUT /tasks/{id}); a blank title is a no-op.
    let submit_edit = move || {
        let Some(task_id) = detail.get_untracked() else {
            return;
        };
        let title = edit_title.get_untracked();
        if title.trim().is_empty() || busy.get_untracked() {
            return;
        }
        let body_md = edit_body.get_untracked();
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result =
                rest::update_task(token.as_deref(), &task_id, &EditTask { title, body_md }).await;
            busy.set(false);
            match result {
                Ok(_) => {
                    detail_edit.set(false);
                    load_tasks();
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Delete the selected board (cascades its columns + tasks server-side) after
    // an explicit confirm, then reload so another board auto-selects.
    let delete_board = move || {
        let Some(id) = selected_board.get_untracked() else {
            return;
        };
        if busy.get_untracked() {
            return;
        }
        let name = boards
            .get_untracked()
            .iter()
            .find(|b| b.id == id)
            .map(|b| b.name.clone())
            .unwrap_or_default();
        dialogs.confirm(
            ConfirmSpec::danger(
                "Delete board?",
                format!("Delete the board “{name}” and all of its tasks? This cannot be undone."),
                "Delete",
            ),
            move || {
                busy.set(true);
                let id = id.clone();
                spawn_local(async move {
                    let token = auth::resolve_token();
                    let result = rest::delete_board(token.as_deref(), &id).await;
                    busy.set(false);
                    match result {
                        Ok(()) => {
                            selected_board.set(None);
                            load_boards(true);
                        }
                        Err(e) => error.set(Some(e.to_string())),
                    }
                });
            },
        );
    };

    // Save the open board-rename form (PUT /boards/{id}); a blank name is a no-op.
    let submit_board_rename = move || {
        let Some(id) = selected_board.get_untracked() else {
            return;
        };
        let name = board_rename_name.get_untracked().trim().to_string();
        if name.is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result = rest::rename_board(token.as_deref(), &id, &RenameBoard { name }).await;
            busy.set(false);
            match result {
                Ok(_) => {
                    renaming_board.set(false);
                    load_boards(false);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Append a column to the selected board (the trailing "+ Add column" stub).
    let submit_add_column = move || {
        let Some(board_id) = selected_board.get_untracked() else {
            return;
        };
        let name = new_col_name.get_untracked().trim().to_string();
        if name.is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result = rest::add_column(token.as_deref(), &board_id, &AddColumn { name }).await;
            busy.set(false);
            match result {
                Ok(_) => {
                    new_col_name.set(String::new());
                    add_column_open.set(false);
                    load_boards(false);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Save the open per-column rename; a blank name is a no-op.
    let submit_col_rename = move || {
        let Some(col_id) = renaming_col.get_untracked() else {
            return;
        };
        let name = col_rename_name.get_untracked().trim().to_string();
        if name.is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result =
                rest::rename_column(token.as_deref(), &col_id, &RenameColumn { name }).await;
            busy.set(false);
            match result {
                Ok(_) => {
                    renaming_col.set(None);
                    load_boards(false);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Delete an (empty) column; the server refuses while tasks remain in it or
    // when it is the board's only column, and the banner explains why.
    let do_delete_column = move |col_id: String| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let token = auth::resolve_token();
            let result = rest::delete_column(token.as_deref(), &col_id).await;
            busy.set(false);
            match result {
                Ok(()) => load_boards(false),
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    // Escape backs out one layer: the modal's editor first, then the modal.
    let esc = window_event_listener(leptos::ev::keydown, move |ev| {
        if ev.key() == "Escape" {
            if detail_edit.get_untracked() {
                detail_edit.set(false);
            } else if detail.get_untracked().is_some() {
                detail.set(None);
            }
        }
    });
    on_cleanup(move || esc.remove());

    // The currently-selected board (reactive).
    let current_board = move || {
        let id = selected_board.get()?;
        boards.get().into_iter().find(|b| b.id == id)
    };

    let on_create_board = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        create_board();
    };

    view! {
        <section class="task-panel">
            <header class="task-header">
                <div class="task-header-left">
                    <h2 class="task-title">"Tasks"</h2>
                    <select
                        class="task-board-select"
                        disabled=move || boards.with(Vec::is_empty)
                        on:change=move |ev| {
                            let v = event_target_value(&ev);
                            selected_board.set((!v.is_empty()).then_some(v));
                            add_col.set(None);
                            renaming_col.set(None);
                            add_column_open.set(false);
                            load_tasks();
                        }
                    >
                        <For
                            each=move || boards.get()
                            key=|b| b.id.clone()
                            children=move |b: Board| {
                                let id = b.id.clone();
                                let sel = selected_board.get().as_deref() == Some(id.as_str());
                                let name = b.name.clone();
                                view! { <option value=id.clone() selected=sel>{name}</option> }
                            }
                        />
                    </select>
                    <Show
                        when=move || current_board().is_some()
                        fallback=|| ().into_view()
                    >
                        <button
                            class="task-board-edit"
                            title="Rename this board"
                            disabled=move || busy.get()
                            on:click=move |_| {
                                if let Some(b) = current_board() {
                                    board_rename_name.set(b.name);
                                    renaming_board.set(true);
                                }
                            }
                        >
                            <Icon icon=MdIcon::Edit />
                        </button>
                        <button
                            class="task-board-del"
                            title="Delete this board (and all its tasks)"
                            disabled=move || busy.get()
                            on:click=move |_| delete_board()
                        >
                            <Icon icon=MdIcon::Delete />
                        </button>
                        <input
                            class="task-input task-filter"
                            type="search"
                            placeholder="Filter cards…"
                            aria-label="Filter cards"
                            prop:value=move || filter.get()
                            on:input=move |ev| filter.set(event_target_value(&ev))
                        />
                    </Show>
                </div>
                <form class="task-newboard" on:submit=on_create_board>
                    <input
                        class="task-input"
                        placeholder="New board name…"
                        disabled=move || busy.get()
                        prop:value=move || new_board_name.get()
                        on:input=move |ev| new_board_name.set(event_target_value(&ev))
                    />
                    <button class="task-btn task-btn-primary" type="submit" disabled=move || busy.get()>
                        "Create board"
                    </button>
                </form>
            </header>

            <Show when=move || error.with(Option::is_some) fallback=|| ().into_view()>
                <div class="task-banner task-error">
                    {move || error.get().unwrap_or_default()}
                </div>
            </Show>

            <Show when=move || renaming_board.get() fallback=|| ().into_view()>
                <div class="task-rename-form">
                    <input
                        class="task-input"
                        placeholder="Board name…"
                        disabled=move || busy.get()
                        prop:value=move || board_rename_name.get()
                        on:input=move |ev| board_rename_name.set(event_target_value(&ev))
                        on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                            match ev.key().as_str() {
                                "Enter" => {
                                    ev.prevent_default();
                                    submit_board_rename();
                                }
                                "Escape" => renaming_board.set(false),
                                _ => {}
                            }
                        }
                    />
                    <div class="task-add-actions">
                        <button
                            class="task-btn task-btn-primary"
                            disabled=move || busy.get()
                            on:click=move |_| submit_board_rename()
                        >
                            "Save"
                        </button>
                        <button
                            class="task-btn"
                            disabled=move || busy.get()
                            on:click=move |_| renaming_board.set(false)
                        >
                            "Cancel"
                        </button>
                    </div>
                </div>
            </Show>

            <div class="task-body">
                <Show when=move || loading.get() fallback=|| ().into_view()>
                    <div class="task-status">"Loading…"</div>
                </Show>

                <Show
                    when=move || !loading.get() && boards.with(Vec::is_empty)
                    fallback=|| ().into_view()
                >
                    <div class="task-status">"No boards yet. Create one above to start a Kanban board."</div>
                </Show>

                {move || {
                    let Some(board) = current_board() else {
                        return ().into_any();
                    };
                    let mut cols = board.columns.clone();
                    cols.sort_by_key(|c| c.order);
                    let all_tasks = tasks.get();
                    let needle = filter.get().trim().to_lowercase();
                    let columns = cols
                        .iter()
                        .map(|col| {
                            let col_id = col.id.clone();
                            let col_name = col.name.clone();
                            let col_tasks = tasks_in_column(&all_tasks, &col_id, &needle);
                            let count = col_tasks.len();
                            let cards = col_tasks
                                .iter()
                                .map(|t| {
                                    let tid = t.id.clone();
                                    let open_detail = move || {
                                        detail_edit.set(false);
                                        detail.set(Some(tid.clone()));
                                    };
                                    task_card(
                                        t,
                                        tasks,
                                        dragging,
                                        drag_over,
                                        do_move,
                                        do_status,
                                        open_detail,
                                    )
                                })
                                .collect::<Vec<_>>();
                            // The column header: an inline rename form when this
                            // column's ✎ was clicked, else name + tools + count.
                            let head = {
                                let cid = col_id.clone();
                                let cname = col_name.clone();
                                move || {
                                    if renaming_col.get().as_deref() == Some(cid.as_str()) {
                                        view! {
                                            <input
                                                class="task-input task-col-rename"
                                                placeholder="Column name…"
                                                disabled=move || busy.get()
                                                prop:value=move || col_rename_name.get()
                                                on:input=move |ev| col_rename_name.set(event_target_value(&ev))
                                                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                                                    match ev.key().as_str() {
                                                        "Enter" => {
                                                            ev.prevent_default();
                                                            submit_col_rename();
                                                        }
                                                        "Escape" => renaming_col.set(None),
                                                        _ => {}
                                                    }
                                                }
                                            />
                                            <button
                                                class="task-icon"
                                                type="button"
                                                title="Save column name"
                                                disabled=move || busy.get()
                                                on:click=move |_| submit_col_rename()
                                            >
                                                <Icon icon=MdIcon::Check />
                                            </button>
                                            <button
                                                class="task-icon"
                                                type="button"
                                                title="Cancel"
                                                on:click=move |_| renaming_col.set(None)
                                            >
                                                <Icon icon=MdIcon::Close />
                                            </button>
                                        }
                                        .into_any()
                                    } else {
                                        let rename_id = cid.clone();
                                        let rename_seed = cname.clone();
                                        let del_id = cid.clone();
                                        let del_name = cname.clone();
                                        view! {
                                            <span class="task-col-name">{cname.clone()}</span>
                                            <span class="task-col-tools">
                                                <button
                                                    class="task-icon"
                                                    type="button"
                                                    title="Rename column"
                                                    disabled=move || busy.get()
                                                    on:click=move |_| {
                                                        col_rename_name.set(rename_seed.clone());
                                                        renaming_col.set(Some(rename_id.clone()));
                                                    }
                                                >
                                                    <Icon icon=MdIcon::Edit />
                                                </button>
                                                <button
                                                    class="task-icon task-icon-del"
                                                    type="button"
                                                    title="Delete column (it must be empty)"
                                                    disabled=move || busy.get()
                                                    on:click=move |_| {
                                                        let del_id = del_id.clone();
                                                        dialogs.confirm(
                                                            ConfirmSpec::danger(
                                                                "Delete column?",
                                                                format!(
                                                                    "Delete the column “{del_name}”? It must be empty."
                                                                ),
                                                                "Delete",
                                                            ),
                                                            move || do_delete_column(del_id.clone()),
                                                        );
                                                    }
                                                >
                                                    <Icon icon=MdIcon::Delete />
                                                </button>
                                            </span>
                                            <span class="task-col-count">{count}</span>
                                        }
                                        .into_any()
                                    }
                                }
                            };
                            let col_id_for_add = col_id.clone();
                            let is_adding = {
                                let cid = col_id.clone();
                                move || add_col.get().as_deref() == Some(cid.as_str())
                            };
                            // Drop-target wiring: hovering this column's background
                            // highlights it; dropping there appends the card (cards
                            // themselves catch insert-above drops and stop the event).
                            let col_drop = col_id.clone();
                            let col_over = col_id.clone();
                            let col_cls = col_id.clone();
                            let col_class = move || {
                                if drag_over.get().is_some_and(|(c, _)| c == col_cls) {
                                    "task-col task-col-drop"
                                } else {
                                    "task-col"
                                }
                            };
                            view! {
                                <div
                                    class=col_class
                                    on:dragover=move |ev: leptos::ev::DragEvent| {
                                        ev.prevent_default();
                                        let want = Some((col_over.clone(), None));
                                        if drag_over.get_untracked() != want {
                                            drag_over.set(want);
                                        }
                                    }
                                    on:drop=move |ev: leptos::ev::DragEvent| {
                                        ev.prevent_default();
                                        drag_over.set(None);
                                        if let Some(tid) = dragging.get_untracked() {
                                            dragging.set(None);
                                            // A background drop appends — skip only when the
                                            // card is already the column's last.
                                            let already_last = tasks
                                                .get_untracked()
                                                .iter()
                                                .rfind(|t| t.column_id == col_drop)
                                                .is_some_and(|t| t.id == tid);
                                            if !already_last {
                                                do_move(tid, col_drop.clone(), None);
                                            }
                                        }
                                    }
                                >
                                    <div class="task-col-head">{head}</div>
                                    <div class="task-col-cards">{cards}</div>
                                    <Show
                                        when=is_adding.clone()
                                        fallback={
                                            let cid = col_id_for_add.clone();
                                            move || {
                                                let cid = cid.clone();
                                                view! {
                                                    <button
                                                        class="task-add-btn"
                                                        disabled=move || busy.get()
                                                        on:click=move |_| {
                                                            add_title.set(String::new());
                                                            add_col.set(Some(cid.clone()));
                                                        }
                                                    >
                                                        "+ Add task"
                                                    </button>
                                                }
                                                .into_any()
                                            }
                                        }
                                    >
                                        <div class="task-add-form">
                                            <input
                                                class="task-input"
                                                placeholder="Task title…"
                                                disabled=move || busy.get()
                                                prop:value=move || add_title.get()
                                                on:input=move |ev| {
                                                    add_title.set(event_target_value(&ev))
                                                }
                                                on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                                                    match ev.key().as_str() {
                                                        "Enter" => {
                                                            ev.prevent_default();
                                                            submit_add();
                                                        }
                                                        "Escape" => add_col.set(None),
                                                        _ => {}
                                                    }
                                                }
                                            />
                                            <div class="task-add-actions">
                                                <button
                                                    class="task-btn task-btn-primary"
                                                    disabled=move || busy.get()
                                                    on:click=move |_| submit_add()
                                                >
                                                    "Add"
                                                </button>
                                                <button
                                                    class="task-btn"
                                                    disabled=move || busy.get()
                                                    on:click=move |_| add_col.set(None)
                                                >
                                                    "Cancel"
                                                </button>
                                            </div>
                                        </div>
                                    </Show>
                                </div>
                            }
                        })
                        .collect::<Vec<_>>();
                    view! {
                        <div class="task-hint">
                            "Drag a card between columns (drop on a card to insert above it); click a card for details."
                        </div>
                        <div class="task-board">
                            {columns}
                            <div class="task-col task-col-ghost">
                                <Show
                                    when=move || add_column_open.get()
                                    fallback=move || {
                                        view! {
                                            <button
                                                class="task-add-btn"
                                                disabled=move || busy.get()
                                                on:click=move |_| {
                                                    new_col_name.set(String::new());
                                                    add_column_open.set(true);
                                                }
                                            >
                                                "+ Add column"
                                            </button>
                                        }
                                        .into_any()
                                    }
                                >
                                    <div class="task-add-form">
                                        <input
                                            class="task-input"
                                            placeholder="Column name…"
                                            disabled=move || busy.get()
                                            prop:value=move || new_col_name.get()
                                            on:input=move |ev| new_col_name.set(event_target_value(&ev))
                                            on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                                                match ev.key().as_str() {
                                                    "Enter" => {
                                                        ev.prevent_default();
                                                        submit_add_column();
                                                    }
                                                    "Escape" => add_column_open.set(false),
                                                    _ => {}
                                                }
                                            }
                                        />
                                        <div class="task-add-actions">
                                            <button
                                                class="task-btn task-btn-primary"
                                                disabled=move || busy.get()
                                                on:click=move |_| submit_add_column()
                                            >
                                                "Add"
                                            </button>
                                            <button
                                                class="task-btn"
                                                disabled=move || busy.get()
                                                on:click=move |_| add_column_open.set(false)
                                            >
                                                "Cancel"
                                            </button>
                                        </div>
                                    </div>
                                </Show>
                            </div>
                        </div>
                    }
                    .into_any()
                }}
            </div>

            {move || {
                let Some(tid) = detail.get() else {
                    return ().into_any();
                };
                let Some(t) = tasks.get().into_iter().find(|x| x.id == tid) else {
                    return ().into_any();
                };
                let cols: Vec<(String, String)> = current_board()
                    .map(|b| {
                        let mut cs = b.columns;
                        cs.sort_by_key(|c| c.order);
                        cs.into_iter().map(|c| (c.id, c.name)).collect()
                    })
                    .unwrap_or_default();
                let is_editing = detail_edit.get();
                let title = t.title.clone();
                let status = t.status.clone();
                let badge_class = format!("task-badge task-badge-{}", status_token(&status));
                let badge_label = status_label(&status);
                let assignee = assignee_label(t.assignee.as_ref());
                let has_assignee = !assignee.trim().is_empty();
                let assignee_chip = format!("👤 {assignee}");
                let body_empty = t.body_md.trim().is_empty();
                let body_html = markdown_html(&t.body_md);
                let cur_col = t.column_id.clone();

                let status_opts = status_options()
                    .into_iter()
                    .map(|(value, label)| {
                        let sel = value == status.as_str();
                        view! { <option value=value selected=sel>{label}</option> }
                    })
                    .collect::<Vec<_>>();
                let col_opts = cols
                    .iter()
                    .map(|(id, name)| {
                        let sel = *id == cur_col;
                        view! { <option value=id.clone() selected=sel>{name.clone()}</option> }
                    })
                    .collect::<Vec<_>>();

                let tid_status = tid.clone();
                let tid_move = tid.clone();
                let cur_col_cmp = cur_col.clone();
                let tid_del = tid.clone();
                let del_title = t.title.clone();
                let seed_title = t.title.clone();
                let seed_body = t.body_md.clone();

                view! {
                    <div
                        class="task-modal-overlay"
                        on:click=move |_| {
                            // Clicking outside closes the viewer, but never an open
                            // editor (that would silently drop the draft).
                            if !detail_edit.get_untracked() {
                                detail.set(None);
                            }
                        }
                    >
                        <div class="task-modal" on:click=move |ev| ev.stop_propagation()>
                            <header class="task-modal-header">
                                {if is_editing {
                                    view! {
                                        <input
                                            class="task-input task-modal-title-input"
                                            placeholder="Task title…"
                                            disabled=move || busy.get()
                                            prop:value=move || edit_title.get()
                                            on:input=move |ev| edit_title.set(event_target_value(&ev))
                                        />
                                    }
                                    .into_any()
                                } else {
                                    view! { <h3 class="task-modal-title">{title.clone()}</h3> }
                                        .into_any()
                                }}
                                <button
                                    class="task-modal-close"
                                    type="button"
                                    title="Close"
                                    aria-label="Close"
                                    on:click=move |_| detail.set(None)
                                >
                                    <Icon icon=MdIcon::Close />
                                </button>
                            </header>
                            <div class="task-modal-body">
                                <div class="task-modal-meta">
                                    <span class=badge_class>{badge_label}</span>
                                    <label class="task-modal-field">
                                        <span class="task-ctl-label">"Status"</span>
                                        <select
                                            class="task-status-select"
                                            prop:value=status.clone()
                                            on:change=move |ev| {
                                                do_status(tid_status.clone(), event_target_value(&ev))
                                            }
                                        >
                                            {status_opts}
                                        </select>
                                    </label>
                                    <label class="task-modal-field">
                                        <span class="task-ctl-label">"Column"</span>
                                        <select
                                            class="task-status-select"
                                            prop:value=cur_col.clone()
                                            on:change=move |ev| {
                                                let v = event_target_value(&ev);
                                                if v != cur_col_cmp {
                                                    do_move(tid_move.clone(), v, None);
                                                }
                                            }
                                        >
                                            {col_opts}
                                        </select>
                                    </label>
                                    {if has_assignee {
                                        view! { <span class="task-assignee">{assignee_chip.clone()}</span> }
                                            .into_any()
                                    } else {
                                        ().into_any()
                                    }}
                                </div>
                                {if is_editing {
                                    view! {
                                        <div class="task-modal-edit">
                                            <MarkdownField
                                                markdown=edit_body
                                                disabled=busy
                                                placeholder="Markdown description…"
                                            />
                                        </div>
                                    }
                                    .into_any()
                                } else if body_empty {
                                    view! {
                                        <div class="task-modal-empty">
                                            "No description. Use Edit to add one."
                                        </div>
                                    }
                                    .into_any()
                                } else {
                                    view! {
                                        <div class="notes-preview task-modal-md" inner_html=body_html></div>
                                    }
                                    .into_any()
                                }}
                            </div>
                            <footer class="task-modal-actions">
                                {if is_editing {
                                    view! {
                                        <button
                                            class="task-btn"
                                            disabled=move || busy.get()
                                            on:click=move |_| detail_edit.set(false)
                                        >
                                            "Cancel"
                                        </button>
                                        <button
                                            class="task-btn task-btn-primary"
                                            disabled=move || busy.get()
                                            on:click=move |_| submit_edit()
                                        >
                                            "Save"
                                        </button>
                                    }
                                    .into_any()
                                } else {
                                    view! {
                                        <button
                                            class="task-btn task-btn-danger"
                                            disabled=move || busy.get()
                                            on:click=move |_| {
                                                let tid_del = tid_del.clone();
                                                dialogs.confirm(
                                                    ConfirmSpec::danger(
                                                        "Delete task?",
                                                        format!("Delete the task “{del_title}”?"),
                                                        "Delete",
                                                    ),
                                                    move || {
                                                        detail.set(None);
                                                        do_delete(tid_del.clone());
                                                    },
                                                );
                                            }
                                        >
                                            "Delete"
                                        </button>
                                        <button
                                            class="task-btn task-btn-primary"
                                            disabled=move || busy.get()
                                            on:click=move |_| {
                                                edit_title.set(seed_title.clone());
                                                edit_body.set(seed_body.clone());
                                                detail_edit.set(true);
                                            }
                                        >
                                            "Edit"
                                        </button>
                                    }
                                    .into_any()
                                }}
                            </footer>
                        </div>
                    </div>
                }
                .into_any()
            }}
        </section>
    }
}

/// Render one task card: a draggable, clickable tile — title + status badge,
/// an assignee chip (when set), a one-line body preview, and a quick status
/// select. Everything else (edit, move, delete) lives in the detail modal the
/// card opens on click / Enter. Hovering it while dragging shows the
/// insert-above indicator; dropping inserts the dragged card above this one.
fn task_card(
    t: &Task,
    tasks: RwSignal<Vec<Task>>,
    dragging: RwSignal<Option<String>>,
    drag_over: RwSignal<DropTarget>,
    do_move: impl Fn(String, String, Option<i32>) + Copy + 'static,
    do_status: impl Fn(String, String) + Copy + 'static,
    open_detail: impl Fn() + Clone + 'static,
) -> impl IntoView {
    let task_id = t.id.clone();
    let col_id = t.column_id.clone();
    let title = t.title.clone();
    let preview = body_preview(&t.body_md);
    let has_preview = !preview.is_empty();
    let status = t.status.clone();
    let badge_class = format!("task-badge task-badge-{}", status_token(&status));
    let badge_label = status_label(&status);
    let assignee = assignee_label(t.assignee.as_ref());
    let has_assignee = !assignee.trim().is_empty();
    let assignee_chip = format!("👤 {assignee}");

    // Status select options.
    let status_for_select = status.clone();
    let options = status_options()
        .into_iter()
        .map(|(value, label)| {
            let selected = value == status.as_str();
            view! { <option value=value selected=selected>{label}</option> }
        })
        .collect::<Vec<_>>();

    let tid_drag = task_id.clone();
    let tid_over = task_id.clone();
    let tid_drop = task_id.clone();
    let tid_status = task_id.clone();
    let col_over = col_id.clone();
    let col_drop = col_id.clone();
    let open_click = open_detail.clone();
    let open_key = open_detail;

    let card_class = {
        let cid = col_id.clone();
        let tid = task_id.clone();
        move || {
            let over = drag_over
                .get()
                .is_some_and(|(c, t)| c == cid && t.as_deref() == Some(tid.as_str()));
            if over {
                "task-card task-card-over"
            } else {
                "task-card"
            }
        }
    };

    view! {
        <div
            class=card_class
            draggable="true"
            role="button"
            tabindex="0"
            on:dragstart=move |_| {
                dragging.set(Some(tid_drag.clone()));
            }
            on:dragend=move |_| {
                dragging.set(None);
                drag_over.set(None);
            }
            on:dragover=move |ev: leptos::ev::DragEvent| {
                ev.prevent_default();
                ev.stop_propagation();
                let want = Some((col_over.clone(), Some(tid_over.clone())));
                if drag_over.get_untracked() != want {
                    drag_over.set(want);
                }
            }
            on:drop=move |ev: leptos::ev::DragEvent| {
                ev.prevent_default();
                ev.stop_propagation();
                drag_over.set(None);
                let Some(dragged) = dragging.get_untracked() else {
                    return;
                };
                dragging.set(None);
                if dragged == tid_drop {
                    return;
                }
                // Insert the dragged card **above** this one: its final index is
                // this card's position among the column's tasks with the dragged
                // card taken out (which also makes same-column drags exact).
                let pos = tasks
                    .get_untracked()
                    .iter()
                    .filter(|x| x.column_id == col_drop && x.id != dragged)
                    .position(|x| x.id == tid_drop);
                if let Some(pos) = pos {
                    do_move(dragged, col_drop.clone(), Some(pos as i32));
                }
            }
            on:click=move |_| open_click()
            on:keydown=move |ev: leptos::ev::KeyboardEvent| {
                if ev.key() == "Enter" {
                    ev.prevent_default();
                    open_key();
                }
            }
        >
            <div class="task-card-top">
                <span class="task-card-title">{title}</span>
                <span class=badge_class>{badge_label}</span>
            </div>
            <Show when={ let h = has_assignee; move || h } fallback=|| ().into_view()>
                <div class="task-assignee">{assignee_chip.clone()}</div>
            </Show>
            <Show
                when={ let h = has_preview; move || h }
                fallback=|| ().into_view()
            >
                <div class="task-card-preview">{preview.clone()}</div>
            </Show>
            <div class="task-card-controls" on:click=move |ev| ev.stop_propagation()>
                <span class="task-ctl-label">"Status"</span>
                <select
                    class="task-status-select"
                    aria-label="Status"
                    prop:value=status_for_select.clone()
                    on:change=move |ev| {
                        do_status(tid_status.clone(), event_target_value(&ev))
                    }
                >
                    {options}
                </select>
            </div>
        </div>
    }
}

/// A human-readable label for a task's `assignee` (a free-form JSON value): a bare
/// string as-is; an object's `name` / `display_name` / `id`; otherwise the value's
/// JSON. `None` / `null` → empty (no chip shown).
fn assignee_label(assignee: Option<&serde_json::Value>) -> String {
    let Some(v) = assignee else {
        return String::new();
    };
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(o) => o
            .get("name")
            .or_else(|| o.get("display_name"))
            .or_else(|| o.get("id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string()),
        other => other.to_string(),
    }
}

/// The tasks in a given column that match `needle` (a pre-lowercased filter over
/// title + body; empty = everything), preserving the server's order.
fn tasks_in_column(tasks: &[Task], column_id: &str, needle: &str) -> Vec<Task> {
    tasks
        .iter()
        .filter(|t| t.column_id == column_id && matches_filter(t, needle))
        .cloned()
        .collect()
}

/// Whether a task's title or body contains `needle` (already lowercased);
/// an empty needle matches everything.
fn matches_filter(t: &Task, needle: &str) -> bool {
    needle.is_empty()
        || t.title.to_lowercase().contains(needle)
        || t.body_md.to_lowercase().contains(needle)
}

/// The status select's `(value, label)` options.
fn status_options() -> [(&'static str, &'static str); 4] {
    [
        ("open", "Open"),
        ("in_progress", "In progress"),
        ("blocked", "Blocked"),
        ("done", "Done"),
    ]
}

/// A display label for a status token.
fn status_label(status: &str) -> &'static str {
    match status {
        "in_progress" => "In progress",
        "blocked" => "Blocked",
        "done" => "Done",
        _ => "Open",
    }
}

/// The CSS modifier suffix for a status badge.
fn status_token(status: &str) -> &'static str {
    match status {
        "in_progress" => "progress",
        "blocked" => "blocked",
        "done" => "done",
        _ => "open",
    }
}

/// A one-line preview of a task body: the first non-empty line, markdown markers
/// stripped, capped at 80 chars.
fn body_preview(md: &str) -> String {
    let line = md
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let stripped = line.trim_start_matches(['#', '-', '*', '>', ' ']).trim();
    if stripped.chars().count() > 80 {
        let truncated: String = stripped.chars().take(80).collect();
        format!("{truncated}…")
    } else {
        stripped.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(col: &str, status: &str) -> Task {
        Task {
            id: "t".into(),
            workspace_id: "w".into(),
            board_id: "b".into(),
            column_id: col.into(),
            title: "T".into(),
            body_md: String::new(),
            assignee: None,
            order: 0,
            status: status.into(),
        }
    }

    #[test]
    fn tasks_in_column_filters() {
        let tasks = vec![task("c1", "open"), task("c2", "open"), task("c1", "done")];
        assert_eq!(tasks_in_column(&tasks, "c1", "").len(), 2);
        assert_eq!(tasks_in_column(&tasks, "c2", "").len(), 1);
        assert!(tasks_in_column(&tasks, "c3", "").is_empty());
    }

    #[test]
    fn filter_matches_title_or_body_case_insensitively() {
        let mut a = task("c1", "open");
        a.title = "Deploy the API".into();
        a.body_md = "roll out to staging first".into();
        assert!(matches_filter(&a, ""));
        assert!(matches_filter(&a, "deploy"));
        assert!(matches_filter(&a, "staging"));
        assert!(!matches_filter(&a, "unrelated"));
        let mut b = task("c1", "open");
        b.title = "Other".into();
        let tasks = vec![a, b];
        assert_eq!(tasks_in_column(&tasks, "c1", "deploy").len(), 1);
        assert_eq!(tasks_in_column(&tasks, "c1", "").len(), 2);
    }

    #[test]
    fn status_label_and_token_map() {
        assert_eq!(status_label("in_progress"), "In progress");
        assert_eq!(status_label("done"), "Done");
        assert_eq!(status_label("weird"), "Open");
        assert_eq!(status_token("blocked"), "blocked");
        assert_eq!(status_token("open"), "open");
        assert_eq!(status_token("nonsense"), "open");
    }

    #[test]
    fn assignee_label_renders_value() {
        use serde_json::json;
        assert_eq!(assignee_label(None), "");
        assert_eq!(assignee_label(Some(&json!(null))), "");
        assert_eq!(assignee_label(Some(&json!("alice"))), "alice");
        assert_eq!(assignee_label(Some(&json!({ "name": "Bob" }))), "Bob");
        assert_eq!(
            assignee_label(Some(&json!({ "display_name": "Cara" }))),
            "Cara"
        );
        assert_eq!(assignee_label(Some(&json!({ "id": "u1" }))), "u1");
        assert_eq!(assignee_label(Some(&json!(42))), "42");
    }

    #[test]
    fn body_preview_first_line_capped() {
        assert_eq!(body_preview("# Heading\nbody"), "Heading");
        assert_eq!(body_preview("\n\n- item"), "item");
        assert_eq!(body_preview(""), "");
        let long = "x".repeat(200);
        assert_eq!(body_preview(&long).chars().count(), 81);
    }
}
