use super::*;

fn conv(created_at: &str) -> Conversation {
    Conversation {
        id: created_at.to_string(),
        workspace_id: String::new(),
        title: Some(created_at.to_string()),
        origin: String::new(),
        created_at: created_at.to_string(),
        agent_profile_id: None,
        model: None,
        reasoning_effort: None,
        tags: Vec::new(),
        title_manual: false,
    }
}

fn skill(name: &str) -> Skill {
    Skill {
        id: name.to_string(),
        workspace_id: String::new(),
        name: name.to_string(),
        description: String::new(),
        instructions_md: String::new(),
        tools: Vec::new(),
        code: None,
        advertised: true,
    }
}

#[test]
fn transient_rest_errors_retry_definitive_ones_do_not() {
    use crate::rest::RestError;
    assert!(transient_rest_error(&RestError::Transport(
        "offline".into()
    )));
    assert!(transient_rest_error(&RestError::Status {
        status: 502,
        message: String::new(),
    }));
    assert!(transient_rest_error(&RestError::Status {
        status: 429,
        message: String::new(),
    }));
    // A definitive rejection (no STT model, bad audio) fails identically on
    // retry — it must surface at once.
    assert!(!transient_rest_error(&RestError::Status {
        status: 400,
        message: String::new(),
    }));
    assert!(!transient_rest_error(&RestError::Status {
        status: 415,
        message: String::new(),
    }));
    assert!(!transient_rest_error(&RestError::Decode("bad json".into())));
}

#[test]
fn every_app_authoring_tool_mounts_inline_without_a_tool_card() {
    assert!(is_ui_authoring_tool("present_ui"));
    assert!(is_ui_authoring_tool("create_ui_components"));
    assert!(is_ui_authoring_tool("edit_ui_components"));
    assert!(is_ui_authoring_tool("edit_ui"));
    assert!(!is_ui_authoring_tool("read_ui"));
}

#[test]
fn voice_input_compression_shortens_and_downmixes_pcm() {
    let left = vec![0.0, 0.2, 0.4, 0.6, 0.8, 1.0];
    let right = vec![0.0, -0.2, -0.4, -0.6, -0.8, -1.0];
    let compressed = compress_pcm(&[left, right], 1.5);
    assert_eq!(compressed.len(), 4, "six frames at 1.5× become four");
    assert!(compressed.iter().all(|sample| sample.abs() < f32::EPSILON));

    let mono = compress_pcm(&[(0..9).map(|n| n as f32 / 10.0).collect()], 1.5);
    assert_eq!(mono.len(), 6);
    assert!((mono[1] - 0.15).abs() < 0.001);
}

#[test]
fn compressed_pcm_encodes_a_valid_mono_wav() {
    let wav = pcm16_wav(&[-1.0, 0.0, 1.0], 48_000).unwrap();
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 48_000);
    assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
    assert_eq!(wav.len(), 50);
}

#[test]
fn speech_capability_is_not_ready_until_a_nonempty_catalog_arrives() {
    assert!(!SpeechCapability::Checking.ready());
    assert!(!SpeechCapability::from_model_count(0).ready());
    assert!(SpeechCapability::from_model_count(1).ready());
}

#[test]
fn tool_only_rounds_do_not_render_an_empty_prose_bubble() {
    assert!(!should_render_message_text("", false, true));
    // The bubble also disappears live as soon as the tool card arrives.
    assert!(!should_render_message_text(" \n ", true, true));
}

#[test]
fn empty_streaming_assistant_keeps_its_initial_waiting_bubble() {
    assert!(should_render_message_text("", true, false));
    assert!(should_render_message_text(
        "Calling the search tool…",
        false,
        true
    ));
}

#[test]
fn chat_bottom_detection_allows_rounding_but_not_a_reader_scroll_up() {
    assert!(chat_is_at_bottom(500, 1_000, 500));
    assert!(chat_is_at_bottom(500 - CHAT_BOTTOM_SLOP_PX, 1_000, 500));
    assert!(!chat_is_at_bottom(499 - CHAT_BOTTOM_SLOP_PX, 1_000, 500));
}

#[test]
fn parse_slash_command_recognizes_new_with_and_without_opener() {
    assert_eq!(
        parse_slash_command("/new", &[]),
        Some((SlashCmd::New, String::new()))
    );
    assert_eq!(
        parse_slash_command("  /NEW plan my week  ", &[]),
        Some((SlashCmd::New, "plan my week".to_string()))
    );
}

#[test]
fn parse_slash_command_matches_skills_and_extracts_args() {
    let skills = [skill("summarize"), skill("triage inbox")];
    assert_eq!(
        parse_slash_command("/summarize", &skills),
        Some((SlashCmd::Skill("summarize".to_string()), String::new()))
    );
    assert_eq!(
        parse_slash_command("/summarize the meeting notes", &skills),
        Some((
            SlashCmd::Skill("summarize".to_string()),
            "the meeting notes".to_string()
        ))
    );
    // A spaced name matches whole, not just its first token.
    assert_eq!(
        parse_slash_command("/triage inbox from today", &skills),
        Some((
            SlashCmd::Skill("triage inbox".to_string()),
            "from today".to_string()
        ))
    );
    // Hand-typed case still resolves to the canonical name.
    assert_eq!(
        parse_slash_command("/Summarize this", &skills),
        Some((SlashCmd::Skill("summarize".to_string()), "this".to_string()))
    );
}

#[test]
fn parse_slash_command_prefers_longest_name_and_new_over_a_skill() {
    let skills = [skill("sum"), skill("sum it up"), skill("new")];
    assert_eq!(
        parse_slash_command("/sum it up quickly", &skills),
        Some((
            SlashCmd::Skill("sum it up".to_string()),
            "quickly".to_string()
        ))
    );
    // The built-in wins over a skill that happens to be named "new".
    assert_eq!(
        parse_slash_command("/new", &skills),
        Some((SlashCmd::New, String::new()))
    );
}

#[test]
fn parse_slash_command_passes_through_non_commands() {
    let skills = [skill("summarize")];
    assert_eq!(parse_slash_command("hello", &skills), None);
    assert_eq!(parse_slash_command("/etc/hosts is odd", &skills), None);
    assert_eq!(parse_slash_command("/", &skills), None);
}

#[test]
fn fmt_cost_formats_and_floors_tiny_costs() {
    assert_eq!(fmt_cost(0.0), "$0.0000");
    assert_eq!(fmt_cost(0.0123), "$0.0123");
    assert_eq!(fmt_cost(1.5), "$1.5000");
    // A nonzero cost below 4-decimal resolution is flagged, not rounded to zero.
    assert_eq!(fmt_cost(0.00002), "<$0.0001");
}

#[test]
fn fmt_int_groups_thousands() {
    assert_eq!(fmt_int(0), "0");
    assert_eq!(fmt_int(42), "42");
    assert_eq!(fmt_int(1_000), "1,000");
    assert_eq!(fmt_int(12_345), "12,345");
    assert_eq!(fmt_int(1_234_567), "1,234,567");
}

#[test]
fn token_tooltip_includes_turn_cache_and_running_total() {
    let t = TurnTokens {
        prompt_tokens: 1_200,
        completion_tokens: 340,
        total_tokens: 1_540,
        cached_tokens: 800,
        cache_creation_tokens: 0,
    };
    let tip = fmt_token_tooltip(&t, 5_402);
    assert_eq!(
        tip,
        "This turn: 1,200 in + 340 out = 1,540 tokens\nCache: 800 read · 0 written\nTotal up to here: 5,402 tokens"
    );
}

#[test]
fn token_tooltip_omits_cache_line_when_no_caching() {
    let t = TurnTokens {
        prompt_tokens: 100,
        completion_tokens: 50,
        total_tokens: 150,
        cached_tokens: 0,
        cache_creation_tokens: 0,
    };
    let tip = fmt_token_tooltip(&t, 150);
    // No cache activity → the cache line is dropped, leaving turn + running total.
    assert_eq!(
        tip,
        "This turn: 100 in + 50 out = 150 tokens\nTotal up to here: 150 tokens"
    );
}

#[test]
fn civil_round_trips() {
    for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (1999, 12, 31), (2026, 6, 18)] {
        let z = days_from_civil(y, m, d);
        assert_eq!(civil_from_days(z), (y, m, d), "round trip {y}-{m}-{d}");
    }
}

#[test]
fn epoch_day_zero_is_1970() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(civil_from_days(0), (1970, 1, 1));
}

#[test]
fn monday_of_week_is_correct() {
    // 2026-06-18 is a Thursday; its week's Monday is 2026-06-15.
    let z = days_from_civil(2026, 6, 18);
    let monday = z - (z + 3).rem_euclid(7);
    assert_eq!(civil_from_days(monday), (2026, 6, 15));
}

#[test]
fn parses_rfc3339_to_epoch_ms() {
    let ms = parse_unix_ms("2026-06-18T09:30:00Z").unwrap();
    let expected = days_from_civil(2026, 6, 18) * 86_400_000 + 9 * 3_600_000 + 30 * 60_000;
    assert_eq!(ms, expected);
    // Tolerates a space separator and fractional seconds; rejects garbage.
    assert!(parse_unix_ms("2026-06-18 09:30:00.123Z").is_some());
    assert!(parse_unix_ms("not-a-date").is_none());
    assert!(parse_unix_ms("").is_none());
}

#[test]
fn classify_splits_today_yesterday_last7_week_month() {
    const DAY: i64 = 86_400_000;
    let now = days_from_civil(2026, 6, 18) * DAY + 12 * 3_600_000;
    assert_eq!(classify(now - 3_600_000, now, 0), Bucket::Today);
    assert_eq!(classify(now + 3_600_000, now, 0), Bucket::Today); // clock skew
    assert_eq!(classify(now - DAY, now, 0), Bucket::Yesterday);
    assert_eq!(classify(now - 2 * DAY, now, 0), Bucket::Last7);
    assert_eq!(classify(now - 6 * DAY, now, 0), Bucket::Last7);
    assert!(matches!(classify(now - 10 * DAY, now, 0), Bucket::Week(_)));
    assert!(matches!(
        classify(now - 100 * DAY, now, 0),
        Bucket::Month(2026, 3)
    ));
}

#[test]
fn classify_uses_local_day_boundaries() {
    const DAY: i64 = 86_400_000;
    const HOUR: i64 = 3_600_000;
    // Now is 00:30 UTC; the item was created an hour earlier, on the
    // previous UTC day. At UTC+2 both fall on the same local day ("Today");
    // at UTC that hour crosses midnight ("Yesterday").
    let now = days_from_civil(2026, 6, 18) * DAY + 30 * 60_000;
    let created = now - HOUR;
    assert_eq!(classify(created, now, 2 * HOUR), Bucket::Today);
    assert_eq!(classify(created, now, 0), Bucket::Yesterday);
}

#[test]
fn group_sessions_labels_and_orders() {
    const DAY: i64 = 86_400_000;
    let now = days_from_civil(2026, 6, 18) * DAY + 12 * 3_600_000;
    let convs = vec![
        conv("2026-06-18T09:00:00Z"), // same day -> Today
        conv("2026-06-17T09:00:00Z"), // 1 day    -> Yesterday
        conv("2026-06-16T09:00:00Z"), // 2 days   -> Previous 7 days
        conv("2026-06-13T09:00:00Z"), // 5 days   -> Previous 7 days
        conv("2026-06-05T09:00:00Z"), // 13 days  -> a week bucket
        conv("2026-03-01T09:00:00Z"), // 109 days -> March 2026
    ];
    let groups = group_sessions(&convs, now, 0);
    // Five distinct groups, in newest-first order.
    assert_eq!(groups.len(), 5);
    assert_eq!(groups[0].label, "Today");
    assert_eq!(groups[1].label, "Yesterday");
    assert_eq!(groups[2].label, "Previous 7 days");
    assert_eq!(groups[2].items.len(), 2);
    assert!(groups[3].label.starts_with("Week of"));
    assert_eq!(groups[4].label, "March 2026");
}

#[test]
fn group_sessions_handles_unparseable_created_at() {
    let groups = group_sessions(&[conv("")], 0, 0);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].label, "Earlier");
}

#[test]
fn group_key_changes_when_group_contents_change() {
    // The sidebar's keyed <For> won't re-render a group whose key is
    // unchanged, so the key must reflect the group's contents — otherwise a
    // newly-added / renamed / removed conversation in an existing recency
    // bucket never appears (the "no chat is added to the sidebar" bug).
    let g = |items: Vec<Conversation>| SessionGroup {
        label: "Last 7 days".to_string(),
        items,
    };
    let a = conv("2026-06-18T09:00:00Z");
    let b = conv("2026-06-17T09:00:00Z");

    // Same label, different membership → different key.
    let one = group_key(&g(vec![a.clone()]));
    let two = group_key(&g(vec![b.clone(), a.clone()]));
    assert_ne!(one, two, "adding a conversation must change the key");

    // A rename (same id, new title) must change the key.
    let mut a_renamed = a.clone();
    a_renamed.title = Some("renamed".to_string());
    assert_ne!(
        group_key(&g(vec![a.clone()])),
        group_key(&g(vec![a_renamed])),
        "renaming a conversation must change the key"
    );

    // Identical contents → identical key (so unchanged groups stay put).
    assert_eq!(group_key(&g(vec![a.clone()])), group_key(&g(vec![a])));
}

#[test]
fn filter_conversations_matches_title_case_insensitively() {
    let convs = vec![conv("Alpha plan"), conv("beta NOTES"), conv("gamma")];
    // Empty / whitespace query → everything (no filtering).
    assert_eq!(filter_conversations(&convs, "  ").len(), 3);
    // Case-insensitive substring on the title (uppercase query, lowercase title).
    let m: Vec<_> = filter_conversations(&convs, "notes")
        .into_iter()
        .map(|c| c.id)
        .collect();
    assert_eq!(m, ["beta NOTES"]);
    assert!(filter_conversations(&convs, "zzz").is_empty());
    // An untitled conversation matches only the empty query.
    let untitled = Conversation {
        id: "u".into(),
        workspace_id: String::new(),
        title: None,
        origin: String::new(),
        created_at: String::new(),
        agent_profile_id: None,
        model: None,
        reasoning_effort: None,
        tags: Vec::new(),
        title_manual: false,
    };
    assert!(filter_conversations(std::slice::from_ref(&untitled), "u").is_empty());
    assert_eq!(filter_conversations(&[untitled], "").len(), 1);
}

#[test]
fn derive_title_trims_and_falls_back() {
    assert_eq!(derive_title("  hello world  "), "hello world");
    assert_eq!(derive_title("\n\nsecond line\nthird"), "second line");
    assert_eq!(derive_title("   "), "New chat");
    let long = "x".repeat(80);
    assert_eq!(derive_title(&long).chars().count(), 50);
}
