//! Per-tool rendering for the chat panel's tool-call cards (SOUL §7/§12).
//!
//! The chat transcript surfaces every tool the assistant runs as a collapsible
//! card. This module is the *renderer registry*: it maps a tool's name plus its
//! (already-captured) arguments and result into a compact, human-readable view.
//!
//! - **Built-in registry tools** (notes, calendar, web fetch/search, memory,
//!   exec, …) get a bespoke card that reads their known fields — e.g. an event
//!   tool shows the event title and time, a web search shows its hit list.
//! - **Everything else** (built-ins without a bespoke card yet, and all external
//!   MCP tools, which are server-prefixed and have an unknown shape) falls back
//!   to a generic "smart" render: the result is pretty-printed JSON when it
//!   parses as JSON, otherwise rendered as sanitized Markdown.
//!
//! App-authoring tools (`present_ui`/`create_ui_components`/
//! `edit_ui_components`/`edit_ui`) are
//! intentionally **not** rendered here — the chat
//! panel mounts those inline as a live [`super::emerged::EmergedUi`] instead, so
//! a card would double-render them.
//!
//! Safety: argument/result strings are untrusted (the result can be arbitrary
//! tool output). They are only ever rendered as escaped Leptos text children or
//! through [`markdown_html`](super::markdown::markdown_html)
//! (which HTML-escapes and link-sanitizes), never injected raw via `inner_html`.

use leptos::prelude::*;
use serde_json::Value;

use super::markdown::markdown_html;

/// A short, human-readable detail for the collapsed card header (next to the tool
/// name) — e.g. the search query, the fetched URL, or the event title. `None`
/// when there's nothing pithy to show, in which case the header is just the name.
#[must_use]
pub fn tool_summary(name: &str, arguments: &str, result: Option<&str>) -> Option<String> {
    let args = parse(arguments);
    let res = result.and_then(parse);
    let detail = match name {
        "create_event" | "read_event" | "update_event" | "delete_event" => res
            .as_ref()
            .and_then(|r| str_field(r, "summary"))
            .or_else(|| args.as_ref().and_then(|a| str_field(a, "summary")))
            .or_else(|| args.as_ref().and_then(|a| str_field(a, "id"))),
        "create_note" | "read_note" | "edit_note" => res
            .as_ref()
            .and_then(|r| str_field(r, "title"))
            .or_else(|| args.as_ref().and_then(|a| str_field(a, "title"))),
        "create_calendar" => res
            .as_ref()
            .and_then(|r| str_field(r, "name"))
            .or_else(|| args.as_ref().and_then(|a| str_field(a, "name"))),
        // `current_time` — show the resolved local time (the `formatted` field);
        // fall back to the requested timezone argument before the call returns.
        "current_time" => res
            .as_ref()
            .and_then(|r| str_field(r, "formatted"))
            .or_else(|| args.as_ref().and_then(|a| str_field(a, "timezone"))),
        "web_search" => args
            .as_ref()
            .and_then(|a| a.get("queries"))
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty()),
        "search_semantic" | "search_files" | "search_emails" | "search_events" => {
            args.as_ref().and_then(|a| str_field(a, "query"))
        }
        "fetch_url" => args.as_ref().and_then(|a| str_field(a, "url")),
        // `ask_user` — the first question, with a count when the form asked more.
        "ask_user" => args
            .as_ref()
            .and_then(|a| a.get("questions"))
            .and_then(Value::as_array)
            .and_then(|qs| {
                let first = qs.first().and_then(|q| str_field(q, "text"))?;
                Some(match qs.len() {
                    0 | 1 => first,
                    n => format!("{first} (+{} more)", n - 1),
                })
            }),
        "run_command" => args
            .as_ref()
            .and_then(|a| str_field(a, "command").or_else(|| str_field(a, "cmd"))),
        "run_javascript" => args
            .as_ref()
            .and_then(|a| str_field(a, "code"))
            .and_then(|code| {
                code.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(str::to_string)
            }),
        "remember" => args.as_ref().and_then(|a| str_field(a, "text")),
        _ => None,
    };
    detail.map(|d| truncate(&d, 80))
}

/// Render the expandable body of a tool card from its arguments and result.
///
/// `result` is `None` while the call is still running; `is_error` flags a failed
/// call (its `result` holds the error payload).
#[must_use]
pub fn render_tool_body(
    name: &str,
    arguments: &str,
    result: Option<&str>,
    is_error: bool,
) -> AnyView {
    let args = parse(arguments);
    let res_val = result.and_then(parse);

    let body = if is_error {
        error_body(result)
    } else {
        match name {
            "create_event" | "read_event" | "update_event" | "delete_event" => {
                event_body(args.as_ref(), res_val.as_ref())
            }
            "create_note" | "read_note" | "edit_note" => note_body(args.as_ref(), res_val.as_ref()),
            "list_notes" => note_list_body(res_val.as_ref()),
            "web_search" => web_search_body(res_val.as_ref()),
            "fetch_url" => fetch_body(res_val.as_ref()),
            "recall" => recall_body(res_val.as_ref()),
            "ask_user" => ask_user_body(args.as_ref(), res_val.as_ref()),
            "run_command" => run_command_body(args.as_ref(), result),
            "run_javascript" => run_javascript_body(args.as_ref(), result, res_val.as_ref()),
            _ => generic_body(args.as_ref(), result, res_val.as_ref()),
        }
    };

    view! { <div class="msg-tool-body">{body}</div> }.into_any()
}

// ---------------------------------------------------------------------------
// Bespoke built-in cards
// ---------------------------------------------------------------------------

fn event_body(args: Option<&Value>, res: Option<&Value>) -> AnyView {
    // Prefer the persisted event in the result; fall back to the call arguments.
    let pick = |key: &str| {
        res.and_then(|r| str_field(r, key))
            .or_else(|| args.and_then(|a| str_field(a, key)))
    };
    let summary = pick("summary").unwrap_or_default();
    let start = pick("start").unwrap_or_default();
    let end = pick("end").unwrap_or_default();
    let location = pick("location");
    let when = match (start.is_empty(), end.is_empty()) {
        (false, false) => format!("{start} → {end}"),
        (false, true) => start,
        _ => String::new(),
    };
    view! {
        <dl class="msg-tool-kv">
            {field("Event", (!summary.is_empty()).then_some(summary))}
            {field("When", (!when.is_empty()).then_some(when))}
            {field("Where", location)}
        </dl>
    }
    .into_any()
}

fn note_body(args: Option<&Value>, res: Option<&Value>) -> AnyView {
    let pick = |key: &str| {
        res.and_then(|r| str_field(r, key))
            .or_else(|| args.and_then(|a| str_field(a, key)))
    };
    let title = pick("title");
    let tags = res
        .and_then(|r| string_array(r, "tags"))
        .or_else(|| args.and_then(|a| string_array(a, "tags")))
        .filter(|t| !t.is_empty())
        .map(|t| t.join(", "));
    let markdown = pick("markdown").filter(|m| !m.is_empty());
    view! {
        <dl class="msg-tool-kv">
            {field("Title", title)}
            {field("Tags", tags)}
        </dl>
        {markdown.map(|m| view! {
            <div class="msg-tool-md" inner_html=markdown_html(&m)></div>
        })}
    }
    .into_any()
}

fn note_list_body(res: Option<&Value>) -> AnyView {
    let titles: Vec<String> = res
        .and_then(|r| r.as_array().cloned())
        .or_else(|| res.and_then(|r| r.get("notes").and_then(Value::as_array).cloned()))
        .unwrap_or_default()
        .iter()
        .filter_map(|n| str_field(n, "title"))
        .collect();
    if titles.is_empty() {
        return generic_body(None, None, res);
    }
    let items: Vec<AnyView> = titles
        .into_iter()
        .map(|t| view! { <li>{t}</li> }.into_any())
        .collect();
    view! { <ul class="msg-tool-list">{items}</ul> }.into_any()
}

/// Render one `SearchResults` object (`{query, provider, results, answer}`) as an
/// optional answer blurb plus the ranked hit list. `None` when it carries no
/// non-empty `results` array (caller decides the fallback).
fn search_results_view(res: &Value) -> Option<AnyView> {
    let results = res.get("results").and_then(Value::as_array)?;
    if results.is_empty() {
        return None;
    }
    let answer = str_field(res, "answer").filter(|a| !a.is_empty());
    let hits: Vec<AnyView> = results
        .iter()
        .map(|hit| {
            let title = str_field(hit, "title").unwrap_or_else(|| "(untitled)".into());
            let url = str_field(hit, "url");
            let snippet = str_field(hit, "snippet").filter(|s| !s.is_empty());
            view! {
                <li class="msg-tool-hit">
                    {match url {
                        Some(u) => view! {
                            <a class="msg-tool-link" href=u.clone() target="_blank"
                                rel="noopener noreferrer">{title}</a>
                        }.into_any(),
                        None => view! { <span>{title}</span> }.into_any(),
                    }}
                    {snippet.map(|s| view! { <div class="msg-tool-snip">{s}</div> })}
                </li>
            }
            .into_any()
        })
        .collect();
    Some(
        view! {
            {answer.map(|a| view! { <div class="msg-tool-md" inner_html=markdown_html(&a)></div> })}
            <ul class="msg-tool-list">{hits}</ul>
        }
        .into_any(),
    )
}

fn web_search_body(res: Option<&Value>) -> AnyView {
    // Batch web search: a `searches` map keyed by query → its result object. Render
    // one labelled group per query, surfacing a per-query `error` (or "no results")
    // where a search came back empty.
    if let Some(map) = res
        .and_then(|r| r.get("searches"))
        .and_then(Value::as_object)
    {
        let groups: Vec<AnyView> = map
            .iter()
            .map(|(query, results)| {
                let body = search_results_view(results).unwrap_or_else(|| {
                    let note = str_field(results, "error")
                        .filter(|e| !e.is_empty())
                        .unwrap_or_else(|| "(no results)".into());
                    view! { <div class="msg-tool-snip">{note}</div> }.into_any()
                });
                view! {
                    <div class="msg-tool-group">
                        <div class="msg-tool-group-title">{query.clone()}</div>
                        {body}
                    </div>
                }
                .into_any()
            })
            .collect();
        return view! { <div class="msg-tool-groups">{groups}</div> }.into_any();
    }
    // Single search: the flat `{query, provider, results, answer}` shape.
    match res {
        Some(r) => search_results_view(r).unwrap_or_else(|| generic_body(None, None, res)),
        None => generic_body(None, None, res),
    }
}

fn fetch_body(res: Option<&Value>) -> AnyView {
    let url = res.and_then(|r| str_field(r, "url"));
    let title = res
        .and_then(|r| str_field(r, "title"))
        .filter(|t| !t.is_empty());
    let status = res
        .and_then(|r| r.get("status"))
        .and_then(Value::as_i64)
        .map(|s| s.to_string());
    let bytes = res
        .and_then(|r| r.get("content_bytes").and_then(Value::as_i64))
        .map(|b| format!("{b} bytes"));
    let content = res
        .and_then(|r| str_field(r, "content"))
        .filter(|c| !c.is_empty());
    view! {
        <dl class="msg-tool-kv">
            {field("URL", url)}
            {field("Title", title)}
            {field("Status", status)}
            {field("Size", bytes)}
        </dl>
        {content.map(|c| view! {
            <pre class="msg-tool-out">{truncate(&c, 4000)}</pre>
        })}
    }
    .into_any()
}

fn recall_body(res: Option<&Value>) -> AnyView {
    let mems = res
        .and_then(|r| r.get("memories"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if mems.is_empty() {
        return view! { <div class="msg-tool-empty">"No memories recalled."</div> }.into_any();
    }
    let items: Vec<AnyView> = mems
        .iter()
        .filter_map(|m| str_field(m, "text"))
        .map(|t| view! { <li>{t}</li> }.into_any())
        .collect();
    view! { <ul class="msg-tool-list">{items}</ul> }.into_any()
}

/// The `ask_user` Q&A card (SOUL §7/§12): one row per question. The questions ride
/// in the call arguments (echoed into the result); the structured `answers` are
/// grafted into the result on transcript replay from the durable question rows
/// (`GET /conversations/{id}/questions`) — when present, each row shows what the
/// user actually picked/typed ("(no answer)" for a skipped question). Without
/// answers (a live turn, or a form superseded unanswered) the offered options show
/// instead.
fn ask_user_body(args: Option<&Value>, res: Option<&Value>) -> AnyView {
    let questions = args
        .and_then(|a| a.get("questions").and_then(Value::as_array).cloned())
        .or_else(|| res.and_then(|r| r.get("questions").and_then(Value::as_array).cloned()))
        .unwrap_or_default();
    if questions.is_empty() {
        return generic_body(args, None, res);
    }
    let answers = res.and_then(|r| r.get("answers").and_then(Value::as_array).cloned());
    let rows: Vec<AnyView> = questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let text = str_field(q, "text").unwrap_or_else(|| format!("Question {}", i + 1));
            // Mirror the server's positional id default (q1, q2, …).
            let qid = str_field(q, "id").unwrap_or_else(|| format!("q{}", i + 1));
            // The user's answer: picked options + any typed text, joined.
            let given = answers.as_ref().map(|list| {
                let a = list
                    .iter()
                    .find(|a| str_field(a, "id").as_deref() == Some(qid.as_str()));
                let mut parts: Vec<String> = a
                    .and_then(|a| a.get("selected").and_then(Value::as_array))
                    .map(|s| {
                        s.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(t) = a
                    .and_then(|a| str_field(a, "text"))
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                {
                    parts.push(t);
                }
                if parts.is_empty() {
                    "(no answer)".to_string()
                } else {
                    parts.join(", ")
                }
            });
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|o| {
                    o.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" · ")
                })
                .filter(|s| !s.is_empty());
            let answer_view = given.map(|g| view! { <div class="msg-tool-answer">{g}</div> });
            let options_view = answer_view
                .is_none()
                .then_some(options)
                .flatten()
                .map(|o| view! { <div class="msg-tool-snip">{o}</div> });
            view! {
                <div class="msg-tool-qa">
                    <div class="msg-tool-question">{text}</div>
                    {options_view}
                    {answer_view}
                </div>
            }
            .into_any()
        })
        .collect();
    view! { <div class="msg-tool-groups">{rows}</div> }.into_any()
}

fn run_command_body(args: Option<&Value>, result: Option<&str>) -> AnyView {
    let cmd = args.and_then(|a| str_field(a, "command").or_else(|| str_field(a, "cmd")));
    // The result is shell output; show it verbatim (escaped) rather than as JSON.
    let out = result.map(|r| truncate(r, 8000));
    view! {
        {cmd.map(|c| view! { <pre class="msg-tool-cmd">"$ "{c}</pre> })}
        {out.map(|o| view! { <pre class="msg-tool-out">{o}</pre> })}
    }
    .into_any()
}

/// JavaScript calls carry a potentially large, multiline function body. Showing
/// their arguments as generic JSON makes that source a quoted string full of
/// `\n` escapes, so render the code verbatim and the optional input separately.
fn run_javascript_body(
    args: Option<&Value>,
    result_raw: Option<&str>,
    result_val: Option<&Value>,
) -> AnyView {
    let Some((code, input)) = javascript_arguments(args) else {
        return generic_body(args, result_raw, result_val);
    };

    view! {
        <div class="msg-tool-sub">"Code"</div>
        <pre class="msg-tool-args">{code}</pre>
        {input.map(|value| view! {
            <div class="msg-tool-sub">"Input"</div>
            <pre class="msg-tool-args">{value}</pre>
        })}
        {generic_body(None, result_raw, result_val)}
    }
    .into_any()
}

// ---------------------------------------------------------------------------
// Generic fallback (unmatched built-ins + all external MCP tools)
// ---------------------------------------------------------------------------

fn generic_body(
    args: Option<&Value>,
    result_raw: Option<&str>,
    result_val: Option<&Value>,
) -> AnyView {
    let args_pretty = args
        .filter(|a| !a.is_null() && !is_empty_object(a))
        .map(pretty);
    // "Smart": a JSON result renders as pretty JSON; non-JSON text renders as
    // sanitized Markdown (which handles plain prose fine).
    let result_view = match (result_val, result_raw) {
        (Some(v), _) => Some(view! { <pre class="msg-tool-out">{truncate(&pretty(v), 8000)}</pre> }.into_any()),
        (None, Some(raw)) if !raw.trim().is_empty() => Some(
            view! { <div class="msg-tool-md" inner_html=markdown_html(&truncate(raw, 8000))></div> }
                .into_any(),
        ),
        _ => None,
    };
    view! {
        {args_pretty.map(|a| view! {
            <div class="msg-tool-sub">"Arguments"</div>
            <pre class="msg-tool-args">{a}</pre>
        })}
        {result_view.map(|r| view! {
            <div class="msg-tool-sub">"Result"</div>
            {r}
        })}
    }
    .into_any()
}

fn error_body(result: Option<&str>) -> AnyView {
    // The error payload is usually a `{"error": "..."}` JSON object; show its
    // message when present, else the raw text.
    let msg = result
        .and_then(parse)
        .and_then(|v| str_field(&v, "error").or_else(|| str_field(&v, "message")))
        .or_else(|| result.map(str::to_string))
        .unwrap_or_else(|| "Tool failed.".into());
    view! { <pre class="msg-tool-err">{msg}</pre> }.into_any()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse a JSON string, returning `None` for empty/invalid input.
fn parse(s: &str) -> Option<Value> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str(t).ok()
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn string_array(v: &Value, key: &str) -> Option<Vec<String>> {
    v.get(key).and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect()
    })
}

fn is_empty_object(v: &Value) -> bool {
    v.as_object().is_some_and(serde_json::Map::is_empty)
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// Extract the two user-facing `run_javascript` arguments. `input` is retained
/// when explicitly set to JSON null, while an omitted input stays hidden.
fn javascript_arguments(args: Option<&Value>) -> Option<(String, Option<String>)> {
    let args = args?.as_object()?;
    let code = args.get("code")?.as_str()?.to_string();
    let input = args.get("input").map(pretty);
    Some((code, input))
}

/// A labelled `<div>`/`<dd>` pair for a key/value list; renders nothing when the
/// value is absent.
fn field(label: &'static str, value: Option<String>) -> Option<AnyView> {
    value.filter(|v| !v.is_empty()).map(|v| {
        view! {
            <div class="msg-tool-row">
                <span class="msg-tool-key">{label}</span>
                <span class="msg-tool-val">{v}</span>
            </div>
        }
        .into_any()
    })
}

/// Truncate to `max` chars on a char boundary, appending an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_prefers_result_then_args() {
        // Event: result summary wins over the argument id.
        let s = tool_summary(
            "create_event",
            r#"{"summary":"Draft"}"#,
            Some(r#"{"summary":"Standup"}"#),
        );
        assert_eq!(s.as_deref(), Some("Standup"));
        // Web search uses the comma-joined `queries` array.
        let s = tool_summary("web_search", r#"{"queries":["rust","tokio"]}"#, None);
        assert_eq!(s.as_deref(), Some("rust, tokio"));
        // Unknown tool: no pithy detail.
        assert_eq!(tool_summary("mcp_server_thing", "{}", None), None);
    }

    #[test]
    fn ask_user_summary_shows_first_question_and_count() {
        let one = tool_summary(
            "ask_user",
            r#"{"questions":[{"text":"Which tone?"}]}"#,
            None,
        );
        assert_eq!(one.as_deref(), Some("Which tone?"));
        let two = tool_summary(
            "ask_user",
            r#"{"questions":[{"text":"Which tone?"},{"text":"Your name?"}]}"#,
            None,
        );
        assert_eq!(two.as_deref(), Some("Which tone? (+1 more)"));
        // No parsable questions → no detail.
        assert_eq!(tool_summary("ask_user", "{}", None), None);
    }

    #[test]
    fn summary_truncates_long_details() {
        let long = "x".repeat(200);
        let args = format!("{{\"queries\":[\"{long}\"]}}");
        let s = tool_summary("web_search", &args, None).unwrap();
        assert_eq!(s.chars().count(), 81); // 80 + ellipsis
        assert!(s.ends_with('…'));
    }

    #[test]
    fn run_javascript_summary_uses_first_nonempty_code_line() {
        let summary = tool_summary(
            "run_javascript",
            r#"{"code":"\n  const answer = input.n * 2;\n  return answer;","input":{"n":21}}"#,
            None,
        );
        assert_eq!(summary.as_deref(), Some("const answer = input.n * 2;"));
    }

    #[test]
    fn run_javascript_arguments_keep_code_verbatim_and_pretty_print_input() {
        let args = parse(r#"{"code":"const n = input.n;\nreturn n * 2;","input":{"n":21}}"#);
        let (code, input) = javascript_arguments(args.as_ref()).unwrap();
        assert_eq!(code, "const n = input.n;\nreturn n * 2;");
        assert_eq!(input.as_deref(), Some("{\n  \"n\": 21\n}"));

        let omitted = parse(r#"{"code":"return 1;"}"#);
        assert_eq!(javascript_arguments(omitted.as_ref()).unwrap().1, None);

        let explicit_null = parse(r#"{"code":"return input;","input":null}"#);
        assert_eq!(
            javascript_arguments(explicit_null.as_ref())
                .unwrap()
                .1
                .as_deref(),
            Some("null")
        );
    }

    #[test]
    fn parse_rejects_empty_and_garbage() {
        assert!(parse("   ").is_none());
        assert!(parse("not json").is_none());
        assert!(parse(r#"{"a":1}"#).is_some());
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 3), "hel…");
        // Multi-byte chars don't panic or split.
        assert_eq!(truncate("héllo", 2), "hé…");
    }
}
