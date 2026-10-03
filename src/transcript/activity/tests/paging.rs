//! `history`: pages of turns read backwards, on the active branch.

use super::super::history::{read_page, History};
use super::*;
use crate::api::schema::AgentHistoryTurn;

// Fixtures. Transcripts link their lines, which paging follows, so these set the links.

/// The line with the link to the one it follows set: `parentUuid` for Claude Code, `parentId`
/// for omp.
fn linked(line: &str, key: &str, parent: Option<&str>) -> String {
    let mut value: Value = serde_json::from_str(line).unwrap();
    value[key] = parent.map_or(Value::Null, |parent| json!(parent));
    value.to_string()
}

fn chain(lines: &[String], id_key: &str, parent_key: &str, start: Option<&str>) -> Vec<String> {
    let mut parent = start.map(str::to_string);
    let mut chained = Vec::new();
    for line in lines {
        chained.push(linked(line, parent_key, parent.as_deref()));
        let value: Value = serde_json::from_str(line).unwrap();
        if let Some(id) = value[id_key].as_str() {
            parent = Some(id.to_string());
        }
    }
    chained
}

/// Claude Code lines in a row, each following the one before, the first following `start`.
fn claude_chain(start: Option<&str>, lines: &[String]) -> Vec<String> {
    chain(lines, "uuid", "parentUuid", start)
}

/// omp entries in a row, each following the one before.
fn omp_chain(lines: &[String]) -> Vec<String> {
    chain(lines, "id", "parentId", None)
}

/// A Claude Code conversation: `turns` turns, each a prompt, a tool call with its result, and
/// an answer. Ids carry the prefix, so that two conversations tell apart.
fn claude_conversation(prefix: &str, turns: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for turn in 1..=turns {
        let id = |kind: &str| format!("{prefix}{kind}{turn}");
        lines.push(claude_user(&id("u"), &format!("Request {turn}")));
        lines.push(claude_call(
            &id("a"),
            &id("m"),
            &id("toolu_"),
            "Bash",
            json!({"command": format!("step {turn}")}),
        ));
        lines.push(claude_result(
            &id("r"),
            &id("toolu_"),
            json!(format!("output {turn}")),
            false,
        ));
        lines.push(claude_text(
            &id("z"),
            &id("n"),
            Some("end_turn"),
            &format!("Answer {turn}"),
        ));
    }
    claude_chain(None, &lines)
}

/// The same for omp.
fn omp_conversation(turns: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for turn in 1..=turns {
        let id = |kind: &str| format!("{kind}{turn}");
        lines.push(omp_user(&id("u"), &format!("Request {turn}")));
        lines.push(omp_response(
            &id("a"),
            "toolUse",
            vec![omp_call(
                &id("call_"),
                "bash",
                json!({"command": format!("step {turn}")}),
            )],
        ));
        lines.push(omp_result(
            &id("r"),
            &id("call_"),
            &format!("output {turn}"),
            false,
        ));
        lines.push(omp_response(
            &id("z"),
            "stop",
            vec![omp_text(&format!("Answer {turn}"))],
        ));
    }
    omp_chain(&lines)
}

fn claude_queued(id: &str, origin: &str, mode: &str, prompt: Value) -> String {
    json!({
        "type": "attachment",
        "uuid": id,
        "timestamp": format!("t-{id}"),
        "attachment": {
            "type": "queued_command",
            "prompt": prompt,
            "commandMode": mode,
            "origin": {"kind": origin},
        },
    })
    .to_string()
}

fn claude_thinking_text(id: &str, response: &str, stop: Option<&str>, text: &str) -> String {
    claude_block(
        id,
        response,
        stop,
        json!({"type": "thinking", "thinking": text, "signature": "sig"}),
    )
}

fn omp_steering(id: &str, text: &str) -> String {
    omp_entry(
        id,
        json!({
            "role": "user",
            "content": [{"type": "text", "text": text}],
            "attribution": "user",
            "steering": true,
        }),
    )
}

fn omp_compaction(id: &str, summary: &str) -> String {
    json!({
        "type": "compaction",
        "id": id,
        "parentId": null,
        "timestamp": format!("t-{id}"),
        "summary": summary,
        "shortSummary": "short",
        "firstKeptEntryId": "none",
        "tokensBefore": 1000,
    })
    .to_string()
}

// Reading.

fn page_of(
    format: TranscriptFormat,
    text: &str,
    before: Option<&str>,
    turns: usize,
    thinking: bool,
) -> Option<History> {
    read_page(format, lines_of(text, READ_CHUNK), before, turns, thinking).unwrap()
}

/// A page of the lines, which has turns.
fn page(format: TranscriptFormat, lines: &[String], before: Option<&str>, turns: usize) -> History {
    page_of(format, &transcript(lines), before, turns, false).expect("a page")
}

fn turn_ids(history: &History) -> Vec<&str> {
    history.turns.iter().map(|turn| turn.id.as_str()).collect()
}

fn entry_ids(turn: &AgentHistoryTurn) -> Vec<&str> {
    turn.entries.iter().map(|entry| entry.id.as_str()).collect()
}

fn entry_kinds(turn: &AgentHistoryTurn) -> Vec<Kind> {
    turn.entries.iter().map(|entry| entry.kind).collect()
}

/// Every turn of a transcript, read `size` at a time from the newest page back to the start.
fn all_turns(format: TranscriptFormat, text: &str, size: usize) -> Vec<AgentHistoryTurn> {
    let mut turns = Vec::new();
    let mut before: Option<String> = None;
    loop {
        let page = page_of(format, text, before.as_deref(), size, false).expect("a page");
        assert!(!page.reset);
        let mut older = page.turns;
        older.append(&mut turns);
        turns = older;
        match page.before {
            Some(next) => before = Some(next),
            None => return turns,
        }
    }
}

// Paging.

#[test]
fn claude_history_pages_back_to_the_start_with_each_turn_once_oldest_first() {
    let lines = claude_conversation("", 7);
    let text = transcript(&lines);

    let newest = page(CLAUDE, &lines, None, 3);
    assert_eq!(turn_ids(&newest), ["u5", "u6", "u7"]);
    assert!(!newest.reset);
    let middle = page(CLAUDE, &lines, newest.before.as_deref(), 3);
    assert_eq!(turn_ids(&middle), ["u2", "u3", "u4"]);
    let oldest = page(CLAUDE, &lines, middle.before.as_deref(), 3);
    assert_eq!(turn_ids(&oldest), ["u1"]);
    assert_eq!(oldest.before, None, "the first turn is the start");

    // However the turns are paged, they are the same turns with the same ids.
    let whole = page(CLAUDE, &lines, None, 7);
    assert_eq!(whole.before, None);
    let paged: Vec<_> = [oldest.turns, middle.turns, newest.turns].concat();
    assert_eq!(whole.turns, paged);
    assert_eq!(all_turns(CLAUDE, &text, 2), paged);
}

#[test]
fn omp_history_pages_back_to_the_start_with_each_turn_once_oldest_first() {
    let lines = omp_conversation(5);
    let text = transcript(&lines);

    let newest = page(OMP, &lines, None, 2);
    assert_eq!(turn_ids(&newest), ["u4", "u5"]);
    let middle = page(OMP, &lines, newest.before.as_deref(), 2);
    assert_eq!(turn_ids(&middle), ["u2", "u3"]);
    let oldest = page(OMP, &lines, middle.before.as_deref(), 2);
    assert_eq!(turn_ids(&oldest), ["u1"]);
    assert_eq!(oldest.before, None);

    let whole = page(OMP, &lines, None, 5);
    assert_eq!(
        whole.turns,
        [oldest.turns, middle.turns, newest.turns].concat()
    );
    assert_eq!(all_turns(OMP, &text, 3), whole.turns);
}

#[test]
fn history_gives_the_current_turn_the_id_and_entries_activity_gives_it() {
    for (format, lines) in [
        (CLAUDE, claude_conversation("", 3)),
        (OMP, omp_conversation(3)),
    ] {
        let history = page(format, &lines, None, 1);
        let turn = &history.turns[0];

        let live = read(format, &lines);

        assert_eq!(live.turn.id.as_deref(), Some(turn.id.as_str()));
        assert_eq!(live.entries, turn.entries);
        assert_eq!(live.turn.started_at, turn.started_at);
        assert_eq!(live.turn.finished, turn.finished);
    }
}

#[test]
fn history_is_the_same_however_the_file_is_chunked() {
    let text = transcript(&claude_conversation("", 6));
    let expected = page_of(CLAUDE, &text, None, 2, false).unwrap();
    let cursor = expected.before.clone().unwrap();
    let expected_next = page_of(CLAUDE, &text, Some(&cursor), 2, false).unwrap();

    for chunk in [7, 64, 1000, READ_CHUNK] {
        let lines = || lines_of(&text, chunk);
        let newest = read_page(CLAUDE, lines(), None, 2, false).unwrap().unwrap();
        assert_eq!(newest, expected, "chunk {chunk}");
        let next = read_page(CLAUDE, lines(), Some(&cursor), 2, false)
            .unwrap()
            .unwrap();
        assert_eq!(next, expected_next, "chunk {chunk}");
    }
}

#[test]
fn history_finishes_where_the_transcript_has_no_prompt() {
    assert_eq!(page_of(CLAUDE, "", None, 5, false), None);
    let bookkeeping = transcript(&[omp_bookkeeping("c1")]);
    assert_eq!(page_of(OMP, &bookkeeping, None, 5, false), None);
}

// A cursor that does not apply.

#[test]
fn history_before_another_transcripts_cursor_answers_the_newest_page_with_reset() {
    let long = claude_conversation("a", 6);
    let cursor = page(CLAUDE, &long, None, 2).before.unwrap();

    // Another conversation of the same shape has a prompt at that very offset, but not this
    // prompt.
    let other = claude_conversation("b", 6);
    let moved = page(CLAUDE, &other, Some(&cursor), 2);
    assert!(moved.reset);
    assert_eq!(turn_ids(&moved), ["bu5", "bu6"]);
    assert!(moved.before.is_some(), "and the page goes on from there");

    // A transcript that lost the turn is no better.
    let shorter = claude_conversation("a", 2);
    let cut = page(CLAUDE, &shorter, Some(&cursor), 5);
    assert!(cut.reset);
    assert_eq!(turn_ids(&cut), ["au1", "au2"]);
}

#[test]
fn history_before_a_cursor_herdr_did_not_issue_answers_the_newest_page_with_reset() {
    let lines = claude_conversation("", 4);
    // One that does not parse, as the cursor of `agent.activity` does not, and one that names
    // the middle of a line.
    for before in ["c:0.5d3b7c1a9e2f4d10.4d2a", "h:5.1"] {
        let answer = page(CLAUDE, &lines, Some(before), 2);
        assert!(answer.reset, "{before:?}");
        assert_eq!(turn_ids(&answer), ["u3", "u4"], "{before:?}");
    }
    // An empty transcript has no page at all; the API answers that as a reset.
    assert_eq!(page_of(CLAUDE, "", Some("h:0.0"), 2, false), None);
}

#[test]
fn history_before_the_oldest_turn_is_an_empty_page_that_is_no_reset() {
    let lines = claude_conversation("", 3);
    // The cursor of the oldest turn, which Herdr issues for no page but a client could keep.
    let first_prompt = format!("h:0.{:x}", fingerprint("u1"));

    let empty = page(CLAUDE, &lines, Some(&first_prompt), 5);

    assert_eq!(empty.turns, []);
    assert_eq!(empty.before, None);
    assert!(!empty.reset);
}

// The size of a page.

#[test]
fn history_page_stops_short_of_half_a_mebibyte_and_the_next_one_goes_on() {
    let big = "x".repeat(100 * 1024);
    let mut lines = Vec::new();
    for turn in 1..=8 {
        lines.push(claude_user(&format!("u{turn}"), "Write it all out"));
        lines.push(claude_text(
            &format!("a{turn}"),
            &format!("m{turn}"),
            Some("end_turn"),
            &big,
        ));
    }
    let lines = claude_chain(None, &lines);

    let newest = page(CLAUDE, &lines, None, 20);

    assert_eq!(turn_ids(&newest), ["u4", "u5", "u6", "u7", "u8"]);
    let bytes: usize = newest
        .turns
        .iter()
        .map(|turn| serde_json::to_vec(turn).unwrap().len())
        .sum();
    assert!(bytes <= 512 * 1024, "{bytes} bytes");
    let older = page(CLAUDE, &lines, newest.before.as_deref(), 20);
    assert_eq!(turn_ids(&older), ["u1", "u2", "u3"]);
    assert_eq!(older.before, None);
}

#[test]
fn history_page_holds_at_least_one_turn_however_large() {
    let huge = "y".repeat(700 * 1024);
    let lines = claude_chain(
        None,
        &[
            claude_user("u1", "Small"),
            claude_text("a1", "m1", Some("end_turn"), "Little."),
            claude_user("u2", "Enormous"),
            claude_text("a2", "m2", Some("end_turn"), &huge),
        ],
    );

    let newest = page(CLAUDE, &lines, None, 5);

    assert_eq!(turn_ids(&newest), ["u2"]);
    assert!(newest.before.is_some());
    let older = page(CLAUDE, &lines, newest.before.as_deref(), 5);
    assert_eq!(turn_ids(&older), ["u1"]);
}

// A turn too long to send whole.

fn claude_turn_with_tools(tools: usize) -> Vec<String> {
    let mut lines = vec![claude_user("u1", "Do everything")];
    for step in 0..tools {
        lines.push(claude_call(
            &format!("a{step}"),
            &format!("m{step}"),
            &format!("toolu_{step}"),
            "Bash",
            json!({"command": format!("step {step}")}),
        ));
        lines.push(claude_result(
            &format!("r{step}"),
            &format!("toolu_{step}"),
            json!("ok"),
            false,
        ));
    }
    lines.push(claude_text("z", "mz", Some("end_turn"), "All done."));
    claude_chain(None, &lines)
}

#[test]
fn history_turn_with_too_many_entries_keeps_its_prompt_and_its_newest() {
    // 250 tool calls make 252 entries with the prompt and the answer.
    let lines = claude_turn_with_tools(250);

    let history = page(CLAUDE, &lines, None, 1);

    let turn = &history.turns[0];
    assert!(turn.truncated);
    assert_eq!(turn.entries.len(), 200);
    assert_eq!(turn.entries[0].kind, Kind::Prompt);
    assert_eq!(turn.entries[0].id, "u1");
    // The 199 entries after the prompt are the newest: tools 52 to 249, then the answer.
    assert_eq!(turn.entries[1].id, "toolu_52");
    assert_eq!(turn.entries[198].id, "toolu_249");
    assert_eq!(turn.entries[199].kind, Kind::Message);
}

#[test]
fn history_turn_of_exactly_two_hundred_entries_is_whole() {
    // The prompt, 198 tool calls and the answer.
    let whole = page(CLAUDE, &claude_turn_with_tools(198), None, 1);
    assert!(!whole.turns[0].truncated);
    assert_eq!(whole.turns[0].entries.len(), 200);

    let cut = page(CLAUDE, &claude_turn_with_tools(199), None, 1);
    assert!(cut.turns[0].truncated);
    assert_eq!(cut.turns[0].entries.len(), 200);
}

// Thinking.

#[test]
fn claude_thinking_is_among_the_entries_only_when_asked_for() {
    let lines = claude_chain(
        None,
        &[
            claude_user("u1", "Go"),
            claude_thinking_text("a1t", "m1", Some("tool_use"), "Weighing the options."),
            claude_text("a1", "m1", Some("tool_use"), "Let me look."),
            claude_call("a2", "m1", "toolu_1", "Bash", json!({"command": "ls"})),
            claude_result("r1", "toolu_1", json!("files"), false),
            // Nothing a reader could show: no words, and redacted.
            claude_thinking_text("a3e", "m2", Some("end_turn"), ""),
            claude_block(
                "a3r",
                "m2",
                Some("end_turn"),
                json!({"type": "redacted_thinking", "data": "AAAA"}),
            ),
            claude_thinking_text("a3t", "m2", Some("end_turn"), "Now to answer."),
            claude_text("a3", "m2", Some("end_turn"), "Done."),
        ],
    );
    let text = transcript(&lines);

    let plain = page_of(CLAUDE, &text, None, 1, false).unwrap();
    let thought = page_of(CLAUDE, &text, None, 1, true).unwrap();

    assert_eq!(
        entry_ids(&plain.turns[0]),
        ["u1", "a1:0", "toolu_1", "a3:0"]
    );
    let turn = &thought.turns[0];
    assert_eq!(
        entry_ids(turn),
        ["u1", "a1t:0", "a1:0", "toolu_1", "a3t:0", "a3:0"]
    );
    assert_eq!(
        entry_kinds(turn),
        [
            Kind::Prompt,
            Kind::Thinking,
            Kind::Note,
            Kind::Tool,
            Kind::Thinking,
            Kind::Message
        ]
    );
    assert_eq!(
        turn.entries[1].text.as_deref(),
        Some("Weighing the options.")
    );
    assert_eq!(turn.entries[4].text.as_deref(), Some("Now to answer."));
    // Asking for thinking adds entries and changes none of the others.
    let without_thinking: Vec<_> = turn
        .entries
        .iter()
        .filter(|entry| entry.kind != Kind::Thinking)
        .cloned()
        .collect();
    assert_eq!(without_thinking, plain.turns[0].entries);

    // The current turn follows suit.
    let live = read_turn(CLAUDE, lines_of(&text, READ_CHUNK), true)
        .unwrap()
        .unwrap()
        .into_activity(None, DEFAULT_LIMIT);
    assert_eq!(ids(&live), entry_ids(turn));
    assert_eq!(ids(&read(CLAUDE, &lines)), entry_ids(&plain.turns[0]));
}

#[test]
fn omp_thinking_is_among_the_entries_only_when_asked_for() {
    let lines = omp_chain(&[
        omp_user("u1", "Go"),
        omp_response(
            "a1",
            "toolUse",
            vec![
                json!({"type": "thinking", "thinking": "Weighing the options."}),
                omp_text("Let me look."),
                omp_call("call_1", "bash", json!({"command": "ls"})),
            ],
        ),
        omp_result("r1", "call_1", "files", false),
        omp_response(
            "a2",
            "stop",
            vec![
                json!({"type": "thinking", "thinking": "   "}),
                json!({"type": "thinking", "thinking": "Now to answer."}),
                omp_text("Done."),
            ],
        ),
    ]);
    let text = transcript(&lines);

    let plain = page_of(OMP, &text, None, 1, false).unwrap();
    let thought = page_of(OMP, &text, None, 1, true).unwrap();

    assert_eq!(entry_ids(&plain.turns[0]), ["u1", "a1:1", "call_1", "a2:2"]);
    let turn = &thought.turns[0];
    assert_eq!(
        entry_ids(turn),
        ["u1", "a1:0", "a1:1", "call_1", "a2:1", "a2:2"]
    );
    assert_eq!(
        entry_kinds(turn),
        [
            Kind::Prompt,
            Kind::Thinking,
            Kind::Note,
            Kind::Tool,
            Kind::Thinking,
            Kind::Message
        ]
    );
    assert_eq!(turn.entries[4].text.as_deref(), Some("Now to answer."));
}

// Compaction.

#[test]
fn claude_compaction_summary_stands_in_its_turn_and_history_goes_on_past_it() {
    let before = claude_chain(
        None,
        &[
            claude_user("u1", "Earlier work"),
            claude_text("a1", "m1", Some("end_turn"), "Earlier answer."),
            claude_user("u2", "Now continue"),
            claude_call("a2", "m2", "toolu_1", "Bash", json!({"command": "ls"})),
            claude_result("r2", "toolu_1", json!("files"), false),
        ],
    );
    // Claude Code cuts the link at a compaction and keeps it as `logicalParentUuid`.
    let boundary = json!({
        "type": "system",
        "subtype": "compact_boundary",
        "uuid": "c0",
        "parentUuid": null,
        "logicalParentUuid": "r2",
        "content": "Conversation compacted",
        "timestamp": "t-c0",
    })
    .to_string();
    let summary = claude_flagged(
        claude_user(
            "c1",
            "This session is being continued.\n\nSummary: parsing is done.",
        ),
        "isCompactSummary",
    );
    let after = claude_chain(
        Some("c1"),
        &[
            claude_text("a3", "m3", Some("end_turn"), "Carrying on."),
            claude_user("u3", "Thanks"),
            claude_text("a4", "m4", Some("end_turn"), "You are welcome."),
        ],
    );
    let lines = [
        before,
        vec![boundary, linked(&summary, "parentUuid", Some("c0"))],
        after,
    ]
    .concat();

    let history = page(CLAUDE, &lines, None, 10);

    // The summary opens no turn, and what came before it is still there.
    assert_eq!(turn_ids(&history), ["u1", "u2", "u3"]);
    let turn = &history.turns[1];
    assert_eq!(entry_ids(turn), ["u2", "toolu_1", "c1", "a3:0"]);
    assert_eq!(
        entry_kinds(turn),
        [Kind::Prompt, Kind::Tool, Kind::Compaction, Kind::Message]
    );
    assert_eq!(
        turn.entries[2].text.as_deref(),
        Some("This session is being continued.\n\nSummary: parsing is done.")
    );
}

#[test]
fn omp_compaction_stands_in_its_turn_with_its_summary() {
    let lines = omp_chain(&[
        omp_user("u1", "Earlier work"),
        omp_response("a1", "stop", vec![omp_text("Earlier answer.")]),
        omp_user("u2", "Now continue"),
        omp_response(
            "a2",
            "toolUse",
            vec![omp_call("call_1", "bash", json!({"command": "ls"}))],
        ),
        omp_result("r2", "call_1", "files", false),
        omp_compaction("c1", "Parsing is done."),
        omp_response("a3", "stop", vec![omp_text("Carrying on.")]),
    ]);

    let history = page(OMP, &lines, None, 10);

    assert_eq!(turn_ids(&history), ["u1", "u2"]);
    let turn = &history.turns[1];
    assert_eq!(entry_ids(turn), ["u2", "call_1", "c1", "a3:0"]);
    assert_eq!(turn.entries[2].kind, Kind::Compaction);
    assert_eq!(turn.entries[2].text.as_deref(), Some("Parsing is done."));
    assert_eq!(
        ids(&read(OMP, &lines)),
        entry_ids(turn),
        "the current turn has it too"
    );
}

// Images.

#[test]
fn claude_images_are_counted_on_prompts_and_tool_results() {
    let prompt = json!({
        "type": "user",
        "uuid": "u1",
        "timestamp": "t-u1",
        "message": {"role": "user", "content": [
            {"type": "text", "text": "Compare these screenshots"},
            {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
            {"type": "image", "source": {"type": "base64", "data": "BBBB"}},
        ]},
    })
    .to_string();
    let lines = claude_chain(
        None,
        &[
            prompt,
            claude_call(
                "a1",
                "m1",
                "toolu_1",
                "Read",
                json!({"file_path": "/a.png"}),
            ),
            claude_result(
                "r1",
                "toolu_1",
                json!([{"type": "image", "source": {}}, {"type": "text", "text": "shown"}]),
                false,
            ),
            claude_user("u2", "No pictures this time"),
            claude_text("a2", "m2", Some("end_turn"), "Fine."),
        ],
    );

    let history = page(CLAUDE, &lines, None, 2);

    let first = &history.turns[0];
    assert_eq!(first.entries[0].images, Some(2));
    assert_eq!(first.entries[1].images, Some(1));
    assert_eq!(
        first.entries[1].tool.as_ref().unwrap().output.as_deref(),
        Some("shown")
    );
    let second = &history.turns[1];
    assert_eq!(second.entries[0].images, None);
    // The live feed counts them the same.
    let live = read(CLAUDE, &lines[..3]);
    assert_eq!(live.entries[0].images, Some(2));
    assert_eq!(live.entries[1].images, Some(1));
}

#[test]
fn omp_images_are_counted_on_prompts_and_tool_results() {
    let prompt = omp_entry(
        "u1",
        json!({"role": "user", "content": [
            {"type": "text", "text": "What are these?"},
            {"type": "image", "data": "AAAA", "mimeType": "image/png"},
            {"type": "image", "data": "BBBB", "mimeType": "image/png"},
            {"type": "image", "data": "CCCC", "mimeType": "image/png"},
        ]}),
    );
    let screenshot = omp_entry(
        "r1",
        json!({
            "role": "toolResult",
            "toolCallId": "call_1",
            "content": [
                {"type": "text", "text": "captured"},
                {"type": "image", "data": "DDDD", "mimeType": "image/png"},
            ],
            "isError": false,
        }),
    );
    let lines = omp_chain(&[
        prompt,
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call(
                "call_1",
                "browser",
                json!({"action": "screenshot"}),
            )],
        ),
        screenshot,
        omp_response("a2", "stop", vec![omp_text("Three pictures.")]),
    ]);

    let live = read(OMP, &lines);

    assert_eq!(entry(&live, "u1").images, Some(3));
    assert_eq!(entry(&live, "call_1").images, Some(1));
    let history = page(OMP, &lines, None, 1);
    assert_eq!(history.turns[0].entries, live.entries);
}

// Messages sent mid-turn.

#[test]
fn claude_message_sent_mid_turn_is_a_prompt_in_place_and_opens_no_turn() {
    let lines = claude_chain(
        None,
        &[
            claude_user("u1", "Refactor the parser"),
            claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "ls"})),
            claude_result("r1", "toolu_1", json!("files"), false),
            claude_queued(
                "q1",
                "human",
                "prompt",
                json!([
                    {"type": "text", "text": "Also keep the tests green"},
                    {"type": "image", "source": {}},
                ]),
            ),
            claude_call(
                "a2",
                "m2",
                "toolu_2",
                "Bash",
                json!({"command": "cargo test"}),
            ),
            claude_result("r2", "toolu_2", json!("ok"), false),
            // Task notices and other agents' messages arrive the same way and are not the
            // user's words.
            claude_queued(
                "q2",
                "task-notification",
                "task-notification",
                json!("<task-notification>done</task-notification>"),
            ),
            claude_queued("q3", "peer", "prompt", json!("Message from another agent")),
            claude_queued("q4", "human", "bash", json!("ls -la")),
            claude_text("a3", "m3", Some("end_turn"), "Done, tests green."),
        ],
    );

    let history = page(CLAUDE, &lines, None, 5);

    assert_eq!(turn_ids(&history), ["u1"], "no second turn");
    let turn = &history.turns[0];
    assert_eq!(entry_ids(turn), ["u1", "toolu_1", "q1", "toolu_2", "a3:0"]);
    assert_eq!(
        entry_kinds(turn),
        [
            Kind::Prompt,
            Kind::Tool,
            Kind::Prompt,
            Kind::Tool,
            Kind::Message
        ]
    );
    assert_eq!(
        turn.entries[2].text.as_deref(),
        Some("Also keep the tests green")
    );
    assert_eq!(turn.entries[2].images, Some(1));
    // The current turn shows it where it was written too.
    let live = read(CLAUDE, &lines);
    assert_eq!(live.turn.id.as_deref(), Some("u1"));
    assert_eq!(ids(&live), entry_ids(turn));
}

#[test]
fn a_message_sent_mid_turn_comes_through_since_once_and_in_place() {
    let early = claude_chain(
        None,
        &[
            claude_user("u1", "Go"),
            claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "ls"})),
        ],
    );
    let first = read(CLAUDE, &early);
    // The message arrives with the tool's result.
    let late = claude_chain(
        Some("a1"),
        &[
            claude_queued("q1", "human", "prompt", json!("Use the other flag")),
            claude_result("r1", "toolu_1", json!("ok"), false),
        ],
    );
    let lines = [early, late].concat();

    let second = read_since(CLAUDE, &lines, &first.cursor);

    assert_eq!(ids(&second), ["toolu_1", "q1"]);
    assert_eq!(entry(&second, "q1").kind, Kind::Prompt);
    assert_eq!(text(&second, "q1"), "Use the other flag");
    assert!(!second.reset);
    let third = read_since(CLAUDE, &lines, &second.cursor);
    assert!(third.entries.is_empty(), "{:?}", ids(&third));
}

#[test]
fn omp_message_sent_mid_turn_is_a_prompt_in_place_and_opens_no_turn() {
    let lines = omp_chain(&[
        omp_user("u1", "Add the endpoint"),
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call("call_1", "bash", json!({"command": "ls"}))],
        ),
        omp_result("r1", "call_1", "files", false),
        omp_steering("s1", "Use the v2 route"),
        omp_response("a2", "stop", vec![omp_text("Added it on v2.")]),
        omp_user("u2", "Thanks"),
        omp_response("a3", "stop", vec![omp_text("Welcome.")]),
    ]);

    let history = page(OMP, &lines, None, 5);

    // What follows the answer is a turn of its own; the steering message is not.
    assert_eq!(turn_ids(&history), ["u1", "u2"]);
    let turn = &history.turns[0];
    assert_eq!(entry_ids(turn), ["u1", "call_1", "s1", "a2:0"]);
    assert_eq!(
        entry_kinds(turn),
        [Kind::Prompt, Kind::Tool, Kind::Prompt, Kind::Message]
    );
    assert_eq!(turn.entries[2].text.as_deref(), Some("Use the v2 route"));
}

// Branches and parallel results.

#[test]
fn omp_history_shows_the_active_branch_only() {
    let mut lines = vec![
        omp_user("u1", "First request"),
        omp_response("a1", "stop", vec![omp_text("First answer.")]),
        // The user went back to the first answer and asked again; this branch was abandoned.
        omp_user("u2", "Abandoned request"),
        omp_response(
            "a2",
            "toolUse",
            vec![omp_call(
                "call_x",
                "bash",
                json!({"command": "rm -rf build"}),
            )],
        ),
        omp_result("r2", "call_x", "removed", false),
        omp_response("a2z", "stop", vec![omp_text("Abandoned wrap-up.")]),
        // The branch that went on, with a sibling of its own that was dropped.
        omp_user("u3", "Second try"),
        omp_response(
            "a3",
            "toolUse",
            vec![omp_call("call_1", "bash", json!({"command": "ls"}))],
        ),
        omp_result("r3", "call_1", "files", false),
        omp_response("a3z", "stop", vec![omp_text("Dropped wrap-up.")]),
        omp_response("a3b", "stop", vec![omp_text("Final answer.")]),
    ];
    for (index, parent) in [
        (0, None),
        (1, Some("u1")),
        (2, Some("a1")),
        (3, Some("u2")),
        (4, Some("a2")),
        (5, Some("r2")),
        (6, Some("a1")),
        (7, Some("u3")),
        (8, Some("a3")),
        (9, Some("r3")),
        (10, Some("r3")),
    ] {
        lines[index] = linked(&lines[index], "parentId", parent);
    }

    let newest = page(OMP, &lines, None, 1);

    assert_eq!(turn_ids(&newest), ["u3"]);
    assert_eq!(entry_ids(&newest.turns[0]), ["u3", "call_1", "a3b:0"]);
    // Paging back steps over the abandoned turn.
    let older = page(OMP, &lines, newest.before.as_deref(), 1);
    assert_eq!(turn_ids(&older), ["u1"]);
    assert_eq!(entry_ids(&older.turns[0]), ["u1", "a1:0"]);
    assert_eq!(older.before, None);
    let whole = page(OMP, &lines, None, 10);
    assert_eq!(turn_ids(&whole), ["u1", "u3"]);
}

#[test]
fn claude_history_goes_on_past_a_link_to_a_line_the_file_lacks() {
    let mut lines = claude_chain(
        None,
        &[
            claude_user("u1", "Early work"),
            claude_text("a1", "m1", Some("end_turn"), "Early answer."),
            claude_user("u2", "Later work"),
            claude_text("a2", "m2", Some("end_turn"), "Later answer."),
        ],
    );
    // The later conversation follows a line that is not in this file.
    lines[2] = linked(&lines[2], "parentUuid", Some("not-in-the-file"));

    let whole = page(CLAUDE, &lines, None, 10);
    assert_eq!(turn_ids(&whole), ["u1", "u2"]);
    assert_eq!(entry_ids(&whole.turns[0]), ["u1", "a1:0"]);

    // Page by page, the cursor of the later turn leads to the earlier one the same way.
    let newest = page(CLAUDE, &lines, None, 1);
    assert_eq!(turn_ids(&newest), ["u2"]);
    let oldest = page(CLAUDE, &lines, newest.before.as_deref(), 1);
    assert_eq!(turn_ids(&oldest), ["u1"]);
    assert_eq!(oldest.before, None);
}

#[test]
fn claude_history_reaches_a_turn_that_the_session_left_unlinked() {
    // A teammate agent's first instruction is written with no link to what follows: the rest
    // of the conversation starts a chain of its own, with no parent either.
    let first = vec![
        claude_user("u0", "<teammate-message>Take the parser</teammate-message>"),
        json!({"type": "attachment", "uuid": "x1", "attachment": {"type": "session_context"}})
            .to_string(),
    ];
    let second = vec![
        json!({"type": "attachment", "uuid": "h1", "attachment": {"type": "hook_success"}})
            .to_string(),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "ls"})),
        claude_result("r1", "toolu_1", json!("files"), false),
        claude_text("a2", "m2", Some("end_turn"), "Parser taken."),
        claude_user("u1", "Now the printer"),
        claude_text("a3", "m3", Some("end_turn"), "Printer taken."),
    ];
    let lines = [claude_chain(None, &first), claude_chain(None, &second)].concat();

    let history = page(CLAUDE, &lines, None, 10);

    assert_eq!(turn_ids(&history), ["u0", "u1"]);
    assert_eq!(
        entry_ids(&history.turns[0]),
        ["u0", "toolu_1", "a2:0"],
        "the work that followed the instruction is its turn"
    );
    assert_eq!(history.before, None);
    // One page at a time finds it as well.
    let newest = page(CLAUDE, &lines, None, 1);
    let oldest = page(CLAUDE, &lines, newest.before.as_deref(), 1);
    assert_eq!(turn_ids(&oldest), ["u0"]);
    assert_eq!(oldest.before, None);
}

#[test]
fn claude_history_shows_the_active_branch_only() {
    let mut lines = vec![
        claude_user("u1", "First request"),
        claude_text("a1", "m1", Some("end_turn"), "First answer."),
        claude_user("u2", "Abandoned request"),
        claude_text("a2", "m2", Some("end_turn"), "Abandoned answer."),
        // Rewound to the first answer: the same parent again.
        claude_user("u3", "Second try"),
        claude_text("a3", "m3", Some("end_turn"), "Final answer."),
    ];
    for (index, parent) in [
        (0, None),
        (1, Some("u1")),
        (2, Some("a1")),
        (3, Some("u2")),
        (4, Some("a1")),
        (5, Some("u3")),
    ] {
        lines[index] = linked(&lines[index], "parentUuid", parent);
    }

    let history = page(CLAUDE, &lines, None, 10);

    assert_eq!(turn_ids(&history), ["u1", "u3"]);
    assert_eq!(entry_ids(&history.turns[1]), ["u3", "a3:0"]);
}

#[test]
fn claude_history_keeps_the_results_of_parallel_tool_calls() {
    // Claude Code writes each call after the one before and each result after its own call,
    // so the results of all but the last call are off to the side of the conversation.
    let mut lines = vec![
        claude_user("u1", "Check both"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "one"})),
        claude_call("a2", "m1", "toolu_2", "Bash", json!({"command": "two"})),
        claude_result("r1", "toolu_1", json!("first output"), false),
        claude_result("r2", "toolu_2", json!("second output"), false),
        claude_text("a3", "m2", Some("end_turn"), "Both fine."),
        claude_user("u2", "And again"),
        claude_text("a4", "m3", Some("end_turn"), "Sure."),
    ];
    for (index, parent) in [
        (0, None),
        (1, Some("u1")),
        (2, Some("a1")),
        (3, Some("a1")),
        (4, Some("a2")),
        (5, Some("r2")),
        (6, Some("a3")),
        (7, Some("u2")),
    ] {
        lines[index] = linked(&lines[index], "parentUuid", parent);
    }

    let newest = page(CLAUDE, &lines, None, 1);
    assert_eq!(turn_ids(&newest), ["u2"]);
    let older = page(CLAUDE, &lines, newest.before.as_deref(), 1);

    let turn = &older.turns[0];
    assert_eq!(entry_ids(turn), ["u1", "toolu_1", "toolu_2", "a3:0"]);
    let outputs: Vec<_> = turn
        .entries
        .iter()
        .filter_map(|entry| entry.tool.as_ref())
        .map(|tool| (tool.status, tool.output.as_deref()))
        .collect();
    assert_eq!(
        outputs,
        [
            (Status::Succeeded, Some("first output")),
            (Status::Succeeded, Some("second output"))
        ]
    );
    // The live feed, which reads in file order, says the same.
    let live = read(CLAUDE, &lines[..6]);
    assert_eq!(ids(&live), entry_ids(turn));
}

#[test]
fn claude_history_keeps_the_parallel_calls_the_conversation_went_past() {
    // A response that calls two tools is written as one line per call, as siblings. The
    // conversation goes on through one call's result, and the other call's line hangs off to
    // the side with its result: after the one the conversation went through in the first
    // turn, and before it in the second.
    let mut lines = vec![
        claude_user("u1", "Check both"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "one"})),
        claude_call("a2", "m1", "toolu_2", "Bash", json!({"command": "two"})),
        claude_result("r2", "toolu_2", json!("second output"), false),
        claude_result("r1", "toolu_1", json!("first output"), false),
        claude_text("a3", "m2", Some("end_turn"), "Both fine."),
        claude_user("u2", "And two more"),
        claude_call("b1", "m3", "toolu_3", "Bash", json!({"command": "three"})),
        claude_call("b2", "m3", "toolu_4", "Bash", json!({"command": "four"})),
        claude_result("s2", "toolu_4", json!("fourth output"), false),
        claude_result("s1", "toolu_3", json!("third output"), false),
        claude_text("b3", "m4", Some("end_turn"), "Those too."),
    ];
    for (index, parent) in [
        (0, None),
        (1, Some("u1")),
        (2, Some("a1")),
        (3, Some("a2")),
        (4, Some("a1")),
        (5, Some("r1")),
        (6, Some("a3")),
        (7, Some("u2")),
        (8, Some("u2")),
        (9, Some("b2")),
        (10, Some("b1")),
        (11, Some("s2")),
    ] {
        lines[index] = linked(&lines[index], "parentUuid", parent);
    }

    let history = page(CLAUDE, &lines, None, 2);

    assert_eq!(turn_ids(&history), ["u1", "u2"]);
    assert_eq!(
        entry_ids(&history.turns[0]),
        ["u1", "toolu_1", "toolu_2", "a3:0"]
    );
    assert_eq!(
        entry_ids(&history.turns[1]),
        ["u2", "toolu_3", "toolu_4", "b3:0"]
    );
    for turn in &history.turns {
        let finished: Vec<_> = turn
            .entries
            .iter()
            .filter_map(|entry| entry.tool.as_ref())
            .map(|tool| (tool.status, tool.output.is_some()))
            .collect();
        assert_eq!(
            finished,
            [(Status::Succeeded, true), (Status::Succeeded, true)]
        );
    }
}

#[test]
fn claude_history_of_a_running_turn_has_the_results_that_arrived() {
    // The second of two parallel calls is still running; the first has its result, which hangs
    // off the side of the conversation.
    let mut lines = vec![
        claude_user("u1", "Check both"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "one"})),
        claude_call("a2", "m1", "toolu_2", "Bash", json!({"command": "two"})),
        claude_result("r1", "toolu_1", json!("first output"), false),
    ];
    for (index, parent) in [(0, None), (1, Some("u1")), (2, Some("a1")), (3, Some("a1"))] {
        lines[index] = linked(&lines[index], "parentUuid", parent);
    }

    let history = page(CLAUDE, &lines, None, 1);

    let turn = &history.turns[0];
    assert_eq!(entry_ids(turn), ["u1", "toolu_1", "toolu_2"]);
    let statuses: Vec<_> = turn
        .entries
        .iter()
        .filter_map(|entry| entry.tool.as_ref())
        .map(|tool| tool.status)
        .collect();
    assert_eq!(statuses, [Status::Succeeded, Status::Running]);
    assert!(!turn.finished);
}

// A session that opens with a message that is not the user's.

fn omp_developer(id: &str, text: &str) -> String {
    omp_entry(
        id,
        json!({
            "role": "developer",
            "content": [{"type": "text", "text": text}],
            "attribution": "agent",
            "synthetic": true,
        }),
    )
}

/// An omp session whose agent works on the context of a handoff before the user types anything.
fn omp_handoff_session() -> Vec<String> {
    omp_chain(&[
        omp_developer("d1", "Context from the previous session"),
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call("call_1", "bash", json!({"command": "ls"}))],
        ),
        omp_result("r1", "call_1", "files", false),
        omp_response("a2", "stop", vec![omp_text("Ready.")]),
        omp_user("u1", "First request"),
        omp_response("a3", "stop", vec![omp_text("First answer.")]),
        omp_user("u2", "Second request"),
        omp_response("a4", "stop", vec![omp_text("Second answer.")]),
    ])
}

#[test]
fn omp_session_opening_with_context_pages_to_its_start_with_the_context_turn_first() {
    let lines = omp_handoff_session();

    let whole = page(OMP, &lines, None, 10);

    assert_eq!(turn_ids(&whole), ["d1", "u1", "u2"]);
    assert_eq!(whole.before, None);
    let context = &whole.turns[0];
    assert_eq!(entry_ids(context), ["d1", "call_1", "a2:0"]);
    assert_eq!(
        entry_kinds(context),
        [Kind::Context, Kind::Tool, Kind::Message]
    );
    assert_eq!(
        context.entries[0].text.as_deref(),
        Some("Context from the previous session")
    );

    // A page at a time gets there too, and the page that has the context turn is the last.
    let newest = page(OMP, &lines, None, 2);
    assert_eq!(turn_ids(&newest), ["u1", "u2"]);
    let oldest = page(OMP, &lines, newest.before.as_deref(), 2);
    assert_eq!(turn_ids(&oldest), ["d1"]);
    assert_eq!(oldest.before, None);
}

#[test]
fn omp_session_opening_with_context_has_it_as_the_current_turn_before_any_prompt() {
    let lines = omp_chain(&[
        omp_developer("d1", "Context from the previous session"),
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call("call_1", "bash", json!({"command": "ls"}))],
        ),
        omp_result("r1", "call_1", "files", false),
        omp_response("a2", "stop", vec![omp_text("Ready.")]),
    ]);

    let live = read(OMP, &lines);

    assert_eq!(live.turn.id.as_deref(), Some("d1"));
    assert_eq!(kinds(&live), [Kind::Context, Kind::Tool, Kind::Message]);
    assert_eq!(text(&live, "d1"), "Context from the previous session");
}

#[test]
fn later_developer_messages_are_no_part_of_the_conversation() {
    let lines = omp_chain(&[
        omp_developer("d1", "Context from the previous session"),
        omp_response("a1", "stop", vec![omp_text("Ready.")]),
        omp_developer("d2", "Injected before the first prompt"),
        omp_user("u1", "First request"),
        omp_response(
            "a2",
            "toolUse",
            vec![omp_call("call_1", "bash", json!({"command": "ls"}))],
        ),
        omp_developer("d3", "Injected in the middle of the turn"),
        omp_result("r1", "call_1", "files", false),
        omp_response("a3", "stop", vec![omp_text("Done.")]),
    ]);

    let history = page(OMP, &lines, None, 10);

    assert_eq!(turn_ids(&history), ["d1", "u1"]);
    assert_eq!(entry_ids(&history.turns[0]), ["d1", "a1:0"]);
    assert_eq!(entry_ids(&history.turns[1]), ["u1", "call_1", "a3:0"]);
    assert_eq!(ids(&read(OMP, &lines)), ["u1", "call_1", "a3:0"]);
}

#[test]
fn a_developer_message_that_follows_other_work_is_not_the_one_a_session_opened_with() {
    let lines = omp_chain(&[
        omp_response("a0", "stop", vec![omp_text("Resumed.")]),
        omp_developer("d1", "Injected after the session began"),
        omp_response("a1", "stop", vec![omp_text("Noted.")]),
    ]);
    let text = transcript(&lines);

    assert_eq!(page_of(OMP, &text, None, 5, false), None);
    assert!(read_turn(OMP, lines_of(&text, READ_CHUNK), false)
        .unwrap()
        .is_none());
}
