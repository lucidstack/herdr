use std::io;

use serde_json::{json, Value};

use super::*;

type Kind = AgentActivityEntryKind;
type ToolKind = AgentActivityToolKind;
type Status = AgentActivityToolStatus;

const CLAUDE: TranscriptFormat = TranscriptFormat::Claude;
const OMP: TranscriptFormat = TranscriptFormat::Omp;

mod paging;
mod running;

fn lines_of(text: &str, chunk: usize) -> ReverseLines<io::Cursor<Vec<u8>>> {
    ReverseLines::new(io::Cursor::new(text.as_bytes().to_vec()), chunk).unwrap()
}

/// One read of an in-memory transcript.
fn read_text(
    format: TranscriptFormat,
    text: &str,
    since: Option<&str>,
    limit: usize,
) -> Option<Activity> {
    read_turn(format, lines_of(text, READ_CHUNK), false)
        .unwrap()
        .map(|turn| turn.into_activity(since, limit))
}

/// A transcript as a writer leaves it: every line ends with a newline.
fn transcript(lines: &[String]) -> String {
    lines.iter().map(|line| format!("{line}\n")).collect()
}

fn read(format: TranscriptFormat, lines: &[String]) -> Activity {
    read_text(format, &transcript(lines), None, DEFAULT_LIMIT).expect("a turn")
}

fn read_since(format: TranscriptFormat, lines: &[String], since: &str) -> Activity {
    read_text(format, &transcript(lines), Some(since), DEFAULT_LIMIT).expect("a turn")
}

fn ids(activity: &Activity) -> Vec<&str> {
    activity
        .entries
        .iter()
        .map(|entry| entry.id.as_str())
        .collect()
}

fn kinds(activity: &Activity) -> Vec<Kind> {
    activity.entries.iter().map(|entry| entry.kind).collect()
}

fn entry<'a>(activity: &'a Activity, id: &str) -> &'a AgentActivityEntry {
    activity
        .entries
        .iter()
        .find(|entry| entry.id == id)
        .unwrap_or_else(|| panic!("no entry {id} in {:?}", ids(activity)))
}

fn tool<'a>(activity: &'a Activity, id: &str) -> &'a AgentActivityTool {
    entry(activity, id).tool.as_ref().expect("a tool entry")
}

fn text<'a>(activity: &'a Activity, id: &str) -> &'a str {
    entry(activity, id).text.as_deref().expect("a text entry")
}

// Claude Code lines.

fn claude_user(id: &str, text: &str) -> String {
    json!({
        "type": "user",
        "uuid": id,
        "timestamp": format!("t-{id}"),
        "message": {"role": "user", "content": text},
    })
    .to_string()
}

fn claude_user_blocks(id: &str, text: &str) -> String {
    json!({
        "type": "user",
        "uuid": id,
        "timestamp": format!("t-{id}"),
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    })
    .to_string()
}

fn claude_block(id: &str, response: &str, stop: Option<&str>, block: Value) -> String {
    json!({
        "type": "assistant",
        "uuid": id,
        "timestamp": format!("t-{id}"),
        "message": {"id": response, "role": "assistant", "stop_reason": stop, "content": [block]},
    })
    .to_string()
}

fn claude_text(id: &str, response: &str, stop: Option<&str>, text: &str) -> String {
    claude_block(id, response, stop, json!({"type": "text", "text": text}))
}

fn claude_thinking(id: &str, response: &str, stop: Option<&str>) -> String {
    claude_block(
        id,
        response,
        stop,
        json!({"type": "thinking", "thinking": "hmm"}),
    )
}

fn claude_call(id: &str, response: &str, call: &str, name: &str, input: Value) -> String {
    claude_block(
        id,
        response,
        Some("tool_use"),
        json!({"type": "tool_use", "id": call, "name": name, "input": input}),
    )
}

fn claude_result(id: &str, call: &str, content: Value, is_error: bool) -> String {
    json!({
        "type": "user",
        "uuid": id,
        "timestamp": format!("t-{id}"),
        "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": call, "content": content, "is_error": is_error},
        ]},
    })
    .to_string()
}

/// The line with a boolean field set, such as `isSidechain`.
fn claude_flagged(line: String, flag: &str) -> String {
    let mut value: Value = serde_json::from_str(&line).unwrap();
    value[flag] = Value::Bool(true);
    value.to_string()
}

/// The tags Claude Code writes a local slash command in.
fn command_tags(command: &str) -> String {
    format!(
        "<command-name>{command}</command-name>\n<command-message>{}</command-message>\n<command-args></command-args>",
        command.trim_start_matches('/')
    )
}

/// A `system` line of subtype `local_command`, which the newest Claude Code versions write for
/// some local slash commands and for everything they print. It has no message.
fn claude_system_local_command(id: &str, content: &str) -> String {
    json!({
        "type": "system",
        "subtype": "local_command",
        "uuid": id,
        "timestamp": format!("t-{id}"),
        "level": "info",
        "isMeta": false,
        "content": content,
    })
    .to_string()
}

/// The note Claude Code puts before a local slash command, telling the model to ignore it.
fn claude_caveat(id: &str) -> String {
    claude_flagged(
        claude_user(
            id,
            "<local-command-caveat>The messages below were generated by the user while running local commands.</local-command-caveat>",
        ),
        "isMeta",
    )
}

/// A local slash command as a user message.
fn claude_command(id: &str, command: &str) -> String {
    claude_user(id, &command_tags(command))
}

/// A Claude Code tool call on its own, as the feed describes it.
fn claude_described(name: &str, input: Value) -> AgentActivityTool {
    let lines = [
        claude_user("u1", "go"),
        claude_call("a1", "m1", "toolu_1", name, input),
    ];
    tool(&read(CLAUDE, &lines), "toolu_1").clone()
}

/// The output of a Claude Code tool call with the given result content.
fn claude_output(content: Value) -> Option<String> {
    let lines = [
        claude_user("u1", "go"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "run"})),
        claude_result("r1", "toolu_1", content, false),
    ];
    tool(&read(CLAUDE, &lines), "toolu_1").output.clone()
}

// omp lines.

fn omp_entry(id: &str, message: Value) -> String {
    json!({
        "type": "message",
        "id": id,
        "parentId": null,
        "timestamp": format!("t-{id}"),
        "message": message,
    })
    .to_string()
}

fn omp_user(id: &str, text: &str) -> String {
    omp_entry(
        id,
        json!({"role": "user", "content": [{"type": "text", "text": text}]}),
    )
}

fn omp_response(id: &str, stop: &str, content: Vec<Value>) -> String {
    omp_entry(
        id,
        json!({"role": "assistant", "stopReason": stop, "content": content}),
    )
}

fn omp_text(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn omp_call(call: &str, name: &str, arguments: Value) -> Value {
    json!({"type": "toolCall", "id": call, "name": name, "arguments": arguments})
}

fn omp_result(id: &str, call: &str, text: &str, is_error: bool) -> String {
    omp_entry(
        id,
        json!({
            "role": "toolResult",
            "toolCallId": call,
            "content": [{"type": "text", "text": text}],
            "isError": is_error,
        }),
    )
}

fn omp_bookkeeping(id: &str) -> String {
    json!({"type": "custom", "id": id, "parentId": null, "customType": "tool_execution_start"})
        .to_string()
}

/// An omp tool call on its own, as the feed describes it.
fn omp_described(name: &str, arguments: Value) -> AgentActivityTool {
    let lines = [
        omp_user("u1", "go"),
        omp_response("a1", "toolUse", vec![omp_call("call_1", name, arguments)]),
    ];
    tool(&read(OMP, &lines), "call_1").clone()
}

// Turn contents.

#[test]
fn claude_turn_lists_prompt_notes_tools_and_the_final_message_in_order() {
    let lines = [
        claude_user("u0", "An earlier request"),
        claude_text("a0", "m0", Some("end_turn"), "An earlier answer."),
        claude_user("u1", "Fix the flaky test"),
        claude_thinking("a1t", "m1", Some("tool_use")),
        claude_text("a1", "m1", Some("tool_use"), "Let me run it."),
        claude_call(
            "a2",
            "m1",
            "toolu_1",
            "Bash",
            json!({"command": "cargo test\n--quiet", "description": "Run the tests"}),
        ),
        claude_result("r1", "toolu_1", json!("test result: ok"), false),
        claude_thinking("a3t", "m2", Some("end_turn")),
        claude_text("a3", "m2", Some("end_turn"), "Fixed: the test raced."),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(ids(&activity), ["u1", "a1:0", "toolu_1", "a3:0"]);
    assert_eq!(
        kinds(&activity),
        [Kind::Prompt, Kind::Note, Kind::Tool, Kind::Message]
    );
    assert_eq!(text(&activity, "u1"), "Fix the flaky test");
    assert_eq!(text(&activity, "a1:0"), "Let me run it.");
    assert_eq!(text(&activity, "a3:0"), "Fixed: the test raced.");
    let call = tool(&activity, "toolu_1");
    assert_eq!(call.name, "Bash");
    assert_eq!(call.kind, ToolKind::Shell);
    assert_eq!(call.summary, "Run the tests");
    assert_eq!(call.target.as_deref(), Some("cargo test"));
    assert_eq!(call.status, Status::Succeeded);
    assert_eq!(call.output.as_deref(), Some("test result: ok"));
    assert_eq!(activity.turn.started_at.as_deref(), Some("t-u1"));
    assert!(activity.turn.finished);
}

#[test]
fn claude_final_message_joins_the_text_of_its_response() {
    let lines = [
        claude_user("u1", "Explain"),
        claude_text("a1", "m1", Some("end_turn"), "First part."),
        claude_text("a2", "m1", Some("end_turn"), "Second part."),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(ids(&activity), ["u1", "a1:0"]);
    assert_eq!(text(&activity, "a1:0"), "First part.\n\nSecond part.");
    assert_eq!(entry(&activity, "a1:0").kind, Kind::Message);
}

#[test]
fn claude_tool_without_a_result_is_running_and_keeps_the_turn_open() {
    let lines = [
        claude_user("u1", "Build it"),
        claude_call(
            "a1",
            "m1",
            "toolu_1",
            "Bash",
            json!({"command": "make", "description": "Build"}),
        ),
    ];

    let activity = read(CLAUDE, &lines);

    let call = tool(&activity, "toolu_1");
    assert_eq!(call.status, Status::Running);
    assert!(!activity.turn.finished);
}

#[test]
fn claude_error_result_marks_the_tool_failed_and_keeps_its_output() {
    let lines = [
        claude_user("u1", "Build it"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "make"})),
        claude_result("r1", "toolu_1", json!("Exit code 2\nlinker error"), true),
        claude_text("a2", "m2", Some("end_turn"), "The build fails."),
    ];

    let activity = read(CLAUDE, &lines);

    let call = tool(&activity, "toolu_1");
    assert_eq!(call.status, Status::Failed);
    assert_eq!(call.output.as_deref(), Some("Exit code 2\nlinker error"));
    assert!(activity.turn.finished);
}

#[test]
fn claude_pairs_results_with_calls_by_id_wherever_the_result_is_written() {
    let lines = [
        claude_user("u1", "Check both"),
        claude_text("a1", "m1", Some("tool_use"), "Checking both."),
        claude_call(
            "a2",
            "m1",
            "toolu_1",
            "Read",
            json!({"file_path": "/w/one"}),
        ),
        // The first result lands before the second call is even written.
        claude_result("r1", "toolu_1", json!("one"), false),
        claude_call(
            "a3",
            "m1",
            "toolu_2",
            "Read",
            json!({"file_path": "/w/two"}),
        ),
        claude_result("r2", "toolu_2", json!("two"), true),
        claude_text("a4", "m2", Some("end_turn"), "One failed."),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(ids(&activity), ["u1", "a1:0", "toolu_1", "toolu_2", "a4:0"]);
    assert_eq!(tool(&activity, "toolu_1").status, Status::Succeeded);
    assert_eq!(tool(&activity, "toolu_1").output.as_deref(), Some("one"));
    assert_eq!(tool(&activity, "toolu_2").status, Status::Failed);
    assert_eq!(tool(&activity, "toolu_2").output.as_deref(), Some("two"));
}

#[test]
fn claude_feed_leaves_out_thinking_side_chains_and_bookkeeping() {
    let lines = [
        claude_user("u1", "Look around"),
        json!({"type": "attachment", "uuid": "x1", "attachment": {"type": "hook_success"}})
            .to_string(),
        json!({"type": "system", "subtype": "turn_duration", "uuid": "x2"}).to_string(),
        json!({"type": "last-prompt", "lastPrompt": "Look around"}).to_string(),
        claude_thinking("a1t", "m1", Some("end_turn")),
        claude_flagged(
            claude_text("s1", "m9", Some("end_turn"), "Subagent chatter."),
            "isSidechain",
        ),
        claude_flagged(claude_user("s2", "Subagent prompt"), "isSidechain"),
        claude_text("a1", "m1", Some("end_turn"), "Nothing to report."),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(ids(&activity), ["u1", "a1:0"]);
}

#[test]
fn omp_turn_lists_prompt_notes_tools_and_the_final_message_in_order() {
    let lines = [
        omp_user("u0", "An earlier request"),
        omp_response("a0", "stop", vec![omp_text("An earlier answer.")]),
        omp_user("u1", "Add the endpoint"),
        omp_bookkeeping("c1"),
        omp_response(
            "a1",
            "toolUse",
            vec![
                json!({"type": "thinking", "thinking": "hmm"}),
                omp_text("Reading first."),
                omp_call(
                    "call_1",
                    "read",
                    json!({"path": "src/lib.rs", "i": "Reading the entry point"}),
                ),
            ],
        ),
        omp_bookkeeping("c2"),
        omp_result("r1", "call_1", "fn main() {}", false),
        omp_entry(
            "d1",
            json!({"role": "developer", "content": [{"type": "text", "text": "reminder"}]}),
        ),
        omp_response("a2", "stop", vec![omp_text("Endpoint added.")]),
    ];

    let activity = read(OMP, &lines);

    assert_eq!(ids(&activity), ["u1", "a1:1", "call_1", "a2:0"]);
    assert_eq!(
        kinds(&activity),
        [Kind::Prompt, Kind::Note, Kind::Tool, Kind::Message]
    );
    assert_eq!(text(&activity, "u1"), "Add the endpoint");
    assert_eq!(text(&activity, "a1:1"), "Reading first.");
    assert_eq!(text(&activity, "a2:0"), "Endpoint added.");
    let call = tool(&activity, "call_1");
    assert_eq!(call.kind, ToolKind::Read);
    assert_eq!(call.summary, "Reading the entry point");
    assert_eq!(call.target.as_deref(), Some("src/lib.rs"));
    assert_eq!(call.status, Status::Succeeded);
    assert_eq!(call.output.as_deref(), Some("fn main() {}"));
    assert!(activity.turn.finished);
}

#[test]
fn omp_failed_tool_and_running_tool_are_told_apart() {
    let lines = [
        omp_user("u1", "Run both"),
        omp_response(
            "a1",
            "toolUse",
            vec![
                omp_call(
                    "call_1",
                    "bash",
                    json!({"command": "false", "i": "Failing"}),
                ),
                omp_call(
                    "call_2",
                    "bash",
                    json!({"command": "sleep 60", "i": "Waiting"}),
                ),
            ],
        ),
        omp_result("r1", "call_1", "exit 1", true),
    ];

    let activity = read(OMP, &lines);

    assert_eq!(tool(&activity, "call_1").status, Status::Failed);
    assert_eq!(tool(&activity, "call_2").status, Status::Running);
    assert!(!activity.turn.finished);
}

#[test]
fn omp_aborted_response_finishes_the_turn_without_a_message() {
    let lines = [
        omp_user("u1", "Start"),
        omp_response("a1", "aborted", vec![]),
    ];

    let activity = read(OMP, &lines);

    assert_eq!(kinds(&activity), [Kind::Prompt]);
    assert!(activity.turn.finished);
}

#[test]
fn omp_edit_names_the_file_in_its_patch() {
    let call = omp_described(
        "edit",
        json!({
            "i": "Adding the field",
            "input": "[src/api/schema.rs#AB12]\nPUT >3:\n+pub x: u8,\n",
        }),
    );

    assert_eq!(call.kind, ToolKind::Edit);
    assert_eq!(call.summary, "Adding the field");
    assert_eq!(call.target.as_deref(), Some("src/api/schema.rs"));
}

#[test]
fn omp_intent_comes_from_the_call_block_when_the_arguments_have_none() {
    let lines = [
        omp_user("u1", "go"),
        omp_response(
            "a1",
            "toolUse",
            vec![json!({
                "type": "toolCall",
                "id": "call_1",
                "name": "eval",
                "arguments": {"code": "1 + 1", "language": "python"},
                "intent": "Checking the sum",
            })],
        ),
    ];

    let call = tool(&read(OMP, &lines), "call_1").clone();

    assert_eq!(call.kind, ToolKind::Shell);
    assert_eq!(call.summary, "Checking the sum");
}

#[test]
fn omp_tool_names_map_to_kinds_with_or_without_the_underscore_prefix() {
    for (name, kind) in [
        ("bash", ToolKind::Shell),
        ("_bash", ToolKind::Shell),
        ("eval", ToolKind::Shell),
        ("read", ToolKind::Read),
        ("_read", ToolKind::Read),
        ("edit", ToolKind::Edit),
        ("write", ToolKind::Edit),
        ("grep", ToolKind::Search),
        ("_glob", ToolKind::Search),
        ("web_search", ToolKind::Web),
        ("task", ToolKind::Agent),
        ("todo", ToolKind::Todo),
        ("ask", ToolKind::Question),
        ("yield", ToolKind::Other),
        ("mcp__github__search", ToolKind::Other),
        ("mcp__playwright__click", ToolKind::Web),
    ] {
        assert_eq!(omp_described(name, json!({})).kind, kind, "{name}");
    }
}

// Tools.

#[test]
fn claude_tool_names_map_to_kinds() {
    for (name, kind) in [
        ("Bash", ToolKind::Shell),
        ("Read", ToolKind::Read),
        ("Edit", ToolKind::Edit),
        ("Write", ToolKind::Edit),
        ("NotebookEdit", ToolKind::Edit),
        ("Grep", ToolKind::Search),
        ("Glob", ToolKind::Search),
        ("WebFetch", ToolKind::Web),
        ("WebSearch", ToolKind::Web),
        ("Agent", ToolKind::Agent),
        ("Task", ToolKind::Agent),
        ("AskUserQuestion", ToolKind::Question),
        ("TodoWrite", ToolKind::Todo),
        ("Skill", ToolKind::Other),
        ("mcp__jira__getJiraIssue", ToolKind::Other),
        ("mcp__chrome-devtools__click", ToolKind::Web),
    ] {
        assert_eq!(claude_described(name, json!({})).kind, kind, "{name}");
    }
}

#[test]
fn tool_summary_and_target_come_from_the_input() {
    let bash = claude_described(
        "Bash",
        json!({"command": "git status\n&& echo ok", "description": "Show the tree state"}),
    );
    assert_eq!(bash.summary, "Show the tree state");
    assert_eq!(bash.target.as_deref(), Some("git status"));

    let bare = claude_described("Bash", json!({"command": "  ls -la\nwc -l"}));
    assert_eq!(bare.summary, "ls -la");
    assert_eq!(bare.target.as_deref(), Some("ls -la"));

    let read = claude_described("Read", json!({"file_path": "/work/app/src/lib.rs"}));
    assert_eq!(read.summary, "/work/app/src/lib.rs");
    assert_eq!(read.target.as_deref(), Some("/work/app/src/lib.rs"));

    let grep = claude_described("Grep", json!({"pattern": "TODO", "path": "/work/app"}));
    assert_eq!(grep.summary, "TODO");
    assert_eq!(grep.target.as_deref(), Some("/work/app"));

    let glob = claude_described("Glob", json!({"pattern": "**/*.rs"}));
    assert_eq!(glob.summary, "**/*.rs");
    assert_eq!(glob.target.as_deref(), Some("**/*.rs"));

    let fetch = claude_described("WebFetch", json!({"url": "https://example.com/docs"}));
    assert_eq!(fetch.summary, "https://example.com/docs");
    assert_eq!(fetch.target.as_deref(), Some("https://example.com/docs"));

    let search = claude_described("WebSearch", json!({"query": "serde untagged enums"}));
    assert_eq!(search.summary, "serde untagged enums");

    let subagent = claude_described("Agent", json!({"description": "Survey the API"}));
    assert_eq!(subagent.summary, "Survey the API");
}

#[test]
fn tool_summary_is_never_empty_and_falls_back_to_the_name() {
    for input in [json!({}), json!({"command": "   "}), json!("not an object")] {
        let call = claude_described("ExitPlanMode", input);
        assert_eq!(call.summary, "ExitPlanMode");
    }
    assert_eq!(claude_described("", json!({})).summary, "tool");
}

#[test]
fn long_summaries_and_targets_are_cut_to_their_limits() {
    let path = format!("/work/{}src/very_long_file_name.rs", "nested/".repeat(20));
    let read = claude_described("Read", json!({ "file_path": path }));
    assert_eq!(read.summary.chars().count(), 120);
    assert!(read.summary.starts_with('…'));
    assert!(read.summary.ends_with("src/very_long_file_name.rs"));
    // 200 is more than this path, so the target is whole.
    assert_eq!(read.target.as_deref(), Some(path.as_str()));

    let command = format!("echo {}", "x".repeat(400));
    let bash = claude_described("Bash", json!({ "command": command }));
    assert_eq!(bash.summary.chars().count(), 120);
    assert!(bash.summary.ends_with('…'));
    assert!(bash.summary.starts_with("echo xxx"));
    let target = bash.target.unwrap();
    assert_eq!(target.chars().count(), 200);
    assert!(target.ends_with('…'));
}

#[test]
fn ask_tool_carries_its_questions_and_options() {
    let call = claude_described(
        "AskUserQuestion",
        json!({"questions": [
            {
                "question": "Which branch?",
                "header": "Branch",
                "multiSelect": true,
                "options": [{"label": "main", "description": "The stable line"}, {"label": "dev"}],
            },
            {"question": "Proceed?", "options": []},
            {"header": "no question text"},
        ]}),
    );

    assert_eq!(call.kind, ToolKind::Question);
    assert_eq!(call.summary, "AskUserQuestion");
    let question = call.question.expect("questions");
    assert_eq!(question.questions.len(), 2);
    let first = &question.questions[0];
    assert_eq!(first.question, "Which branch?");
    assert_eq!(first.header.as_deref(), Some("Branch"));
    assert!(first.multi_select);
    assert_eq!(first.options.len(), 2);
    assert_eq!(first.options[0].label, "main");
    assert_eq!(
        first.options[0].description.as_deref(),
        Some("The stable line")
    );
    let second = &question.questions[1];
    assert!(!second.multi_select);
}

#[test]
fn omp_ask_carries_its_questions_and_only_questions_have_them() {
    let ask = omp_described(
        "ask",
        json!({"i": "Asking which branch", "questions": [{
            "id": "branch",
            "question": "Which branch?",
            "options": [{"label": "main", "description": "stable"}, "dev"],
            "recommended": 0,
            "multi": true,
        }]}),
    );
    let question = ask.question.expect("questions");
    assert_eq!(question.questions[0].question, "Which branch?");
    assert!(question.questions[0].multi_select);
    let labels: Vec<_> = question.questions[0]
        .options
        .iter()
        .map(|option| option.label.as_str())
        .collect();
    assert_eq!(labels, ["main", "dev"]);

    // The same shape on a call that is not a question tool is not a question.
    let bash = omp_described(
        "bash",
        json!({"command": "ls", "questions": [{"question": "Which?", "options": [{"label": "a"}]}]}),
    );
    assert_eq!(bash.question, None);
}

#[test]
fn output_keeps_the_last_five_lines_and_drops_trailing_blank_ones() {
    let output = claude_output(json!("l1\nl2\nl3\nl4\nl5\nl6\nl7\n\n  \n"));

    assert_eq!(output.as_deref(), Some("l3\nl4\nl5\nl6\nl7"));
}

#[test]
fn output_keeps_the_last_six_hundred_characters_of_a_long_line() {
    let long = format!("{}TAIL", "x".repeat(2000));

    let output = claude_output(json!(long)).unwrap();

    assert_eq!(output.chars().count(), 600);
    assert!(output.starts_with('…'));
    assert!(output.ends_with("xxxxTAIL"));
}

#[test]
fn output_limit_counts_characters_not_bytes() {
    let output = claude_output(json!("é".repeat(1000))).unwrap();

    assert_eq!(output.chars().count(), 600);
    assert!(output.starts_with('…'));
    assert!(output[3..].chars().all(|ch| ch == 'é'));
}

#[test]
fn output_shows_what_a_terminal_would_show() {
    let output = claude_output(json!(
        "\u{1b}[31mFAILED\u{1b}[0m tests::it\n\u{1b}]0;title\u{7}downloading 10%\rdownloading 100%\r\nbell\u{7}\tdone\r\n"
    ));

    assert_eq!(
        output.as_deref(),
        Some("FAILED tests::it\ndownloading 100%\nbell\tdone")
    );
}

#[test]
fn output_is_absent_when_the_result_has_no_text() {
    assert_eq!(claude_output(json!("")), None);
    assert_eq!(claude_output(json!("  \n\n")), None);
    assert_eq!(claude_output(json!([])), None);
    assert_eq!(
        claude_output(json!([{"type": "image", "source": {}}])),
        None
    );
    assert_eq!(claude_output(json!("\u{1b}[2J")), None);
}

#[test]
fn output_joins_the_text_blocks_of_a_result_and_skips_the_rest() {
    let output = claude_output(json!([
        {"type": "text", "text": "one"},
        {"type": "image", "source": {}},
        {"type": "text", "text": "two"},
    ]));

    assert_eq!(output.as_deref(), Some("one\ntwo"));
}

// Which prompt opens the turn.

#[test]
fn claude_turn_starts_at_the_newest_real_prompt() {
    let lines = [
        claude_user("u1", "First task"),
        claude_text("a1", "m1", Some("end_turn"), "First answer."),
        claude_user("u2", "Second task"),
        claude_flagged(
            claude_user("u3", "Base directory for this skill: /x"),
            "isMeta",
        ),
        claude_flagged(claude_user("u4", "Subagent prompt"), "isSidechain"),
        claude_flagged(
            claude_user("u5", "A summary of the conversation"),
            "isCompactSummary",
        ),
        claude_call(
            "a2",
            "m2",
            "toolu_1",
            "Bash",
            json!({"command": "rm -rf /"}),
        ),
        claude_result(
            "r1",
            "toolu_1",
            json!("The user doesn't want to proceed"),
            true,
        ),
        claude_user_blocks("u6", "[Request interrupted by user for tool use]"),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(ids(&activity), ["u2", "u5", "toolu_1"]);
    assert_eq!(text(&activity, "u2"), "Second task");
    assert_eq!(tool(&activity, "toolu_1").status, Status::Failed);
    assert!(activity.turn.finished, "an interrupted turn is over");
}

#[test]
fn claude_local_slash_command_does_not_open_a_turn() {
    let lines = [
        claude_user("u1", "Do the thing"),
        claude_text("a1", "m1", Some("end_turn"), "Done."),
        claude_user(
            "u2",
            "<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>",
        ),
        claude_user("u3", "<local-command-stdout>Set model to fast</local-command-stdout>"),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(ids(&activity), ["u1", "a1:0"]);
    assert!(activity.turn.finished);
}

#[test]
fn claude_local_command_output_written_as_a_system_line_does_not_open_a_turn() {
    for output in [
        "<local-command-stdout>Set model to fast</local-command-stdout>",
        "<local-command-stderr>No such model</local-command-stderr>",
    ] {
        let lines = [
            claude_user("u1", "Do the thing"),
            claude_text("a1", "m1", Some("end_turn"), "Done."),
            claude_caveat("c1"),
            claude_command("u2", "/model"),
            claude_system_local_command("s1", output),
        ];

        let activity = read(CLAUDE, &lines);

        assert_eq!(ids(&activity), ["u1", "a1:0"], "{output}");
        assert!(activity.turn.finished, "{output}");
    }
}

#[test]
fn claude_session_that_only_ran_a_local_command_has_no_activity() {
    let lines = [
        claude_caveat("c1"),
        claude_command("u1", "/clear"),
        claude_system_local_command("s1", "<local-command-stdout></local-command-stdout>"),
    ];

    assert!(read_text(CLAUDE, &transcript(&lines), None, DEFAULT_LIMIT).is_none());
}

#[test]
fn claude_plain_text_slash_command_that_only_printed_output_does_not_open_a_turn() {
    let lines = [
        claude_user("u1", "Do the thing"),
        claude_text("a1", "m1", Some("end_turn"), "Done."),
        claude_caveat("c1"),
        claude_user("u2", "/code-review"),
        claude_system_local_command(
            "s1",
            "<local-command-stdout>Running in the background</local-command-stdout>",
        ),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(ids(&activity), ["u1", "a1:0"]);
    assert!(activity.turn.finished);
}

#[test]
fn claude_prompt_that_starts_with_a_path_is_still_the_turn_once_the_agent_answered_it() {
    // `/model` here is a system line too, as current Claude Code writes it: the command line
    // itself is bookkeeping, and only what it printed follows the answer.
    let lines = [
        claude_user("u1", "/work/app/src/lib.rs does not compile"),
        claude_text("a1", "m1", Some("end_turn"), "A missing import."),
        claude_system_local_command("s1", &command_tags("/model")),
        claude_system_local_command(
            "s2",
            "<local-command-stdout>Set model to fast</local-command-stdout>",
        ),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(ids(&activity), ["u1", "a1:0"]);
    assert_eq!(
        text(&activity, "u1"),
        "/work/app/src/lib.rs does not compile"
    );
    assert!(activity.turn.finished);
}

#[test]
fn claude_interrupted_prompt_stays_the_turn_when_a_local_command_runs_much_later() {
    let lines = [
        claude_user("u1", "/work/app/src/lib.rs does not compile"),
        claude_user_blocks("u2", "[Request interrupted by user]"),
        claude_system_local_command("s1", &command_tags("/model")),
        claude_system_local_command(
            "s2",
            "<local-command-stdout>Set model to fast</local-command-stdout>",
        ),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(ids(&activity), ["u1"]);
    assert!(activity.turn.finished, "the user stopped it");
}

#[test]
fn claude_slash_command_the_model_answered_opens_a_turn_with_its_words() {
    let lines = [
        claude_user("u1", "Old task"),
        claude_text("a1", "m1", Some("end_turn"), "Old answer."),
        claude_user(
            "u2",
            "<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>the diff</command-args>",
        ),
        claude_flagged(claude_user("u3", "Review the diff carefully."), "isMeta"),
        claude_text("a2", "m2", Some("end_turn"), "Looks fine."),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(ids(&activity), ["u2", "a2:0"]);
    assert_eq!(text(&activity, "u2"), "/review the diff");

    // Before the model answers, a command just typed is already the current turn.
    let typed = read(CLAUDE, &lines[..3]);
    assert_eq!(ids(&typed), ["u2"]);
    assert!(!typed.turn.finished);
}

#[test]
fn claude_task_notification_opens_a_turn_with_its_summary() {
    let lines = [
        claude_user("u1", "Run the slow job"),
        claude_text("a1", "m1", Some("end_turn"), "Started it."),
        claude_user(
            "u2",
            "<task-notification>\n<task-id>b1</task-id>\n<summary>Background command \"slow job\" completed (exit code 0)</summary>\n</task-notification>",
        ),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(
        text(&activity, "u2"),
        "Background command \"slow job\" completed (exit code 0)"
    );
}

#[test]
fn claude_prompt_text_leaves_out_attachments_and_reminders() {
    let prompt = json!({
        "type": "user",
        "uuid": "u1",
        "timestamp": "t-u1",
        "message": {"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "data": "AAAA"}},
            {"type": "text", "text": "<system-reminder>be brief</system-reminder>"},
            {"type": "text", "text": "What does this screenshot show?"},
        ]},
    })
    .to_string();

    let activity = read(CLAUDE, &[prompt]);

    assert_eq!(text(&activity, "u1"), "What does this screenshot show?");
}

#[test]
fn omp_turn_starts_at_the_newest_user_message() {
    let lines = [
        omp_user("u1", "First"),
        omp_response("a1", "stop", vec![omp_text("One.")]),
        omp_user("u2", "Second"),
        omp_entry(
            "b1",
            json!({"role": "bashExecution", "command": "ls", "output": "x", "exitCode": 0}),
        ),
    ];

    let activity = read(OMP, &lines);

    assert_eq!(ids(&activity), ["u2"]);
    assert!(!activity.turn.finished);
}

#[test]
fn transcript_without_a_prompt_has_no_activity() {
    let bookkeeping = [
        json!({"type": "session", "id": "s1"}).to_string(),
        omp_response("a1", "stop", vec![omp_text("Hello")]),
    ];

    assert!(read_text(OMP, &transcript(&bookkeeping), None, DEFAULT_LIMIT).is_none());
    assert!(read_text(CLAUDE, "", None, DEFAULT_LIMIT).is_none());
    assert!(read_text(CLAUDE, "\n\nnot json\n", None, DEFAULT_LIMIT).is_none());
}

// A turn cut short.

#[test]
fn interrupted_response_becomes_the_final_message_when_the_marker_arrives() {
    let mut lines = vec![
        claude_user("u1", "Investigate"),
        claude_thinking("a1t", "m1", None),
        claude_text("a1", "m1", None, "Partial finding."),
    ];
    let before = read(CLAUDE, &lines);
    assert_eq!(kinds(&before), [Kind::Prompt, Kind::Note]);
    assert!(!before.turn.finished);

    lines.push(claude_user_blocks("u2", "[Request interrupted by user]"));
    let after = read_since(CLAUDE, &lines, &before.cursor);

    // The text keeps its id and changes kind, so the client replaces the note it holds.
    assert!(!after.reset);
    assert_eq!(ids(&after), ["a1:0"]);
    assert_eq!(entry(&after, "a1:0").kind, Kind::Message);
    assert_eq!(text(&after, "a1:0"), "Partial finding.");
    assert!(after.turn.finished);
}

#[test]
fn interrupt_before_any_response_finishes_the_turn() {
    let lines = [
        claude_user("u1", "Go"),
        claude_user_blocks("u2", "[Request interrupted by user]"),
    ];

    let activity = read(CLAUDE, &lines);

    assert_eq!(kinds(&activity), [Kind::Prompt]);
    assert!(activity.turn.finished);
}

#[test]
fn final_message_turns_into_a_note_when_the_agent_carries_on() {
    let mut lines = vec![
        omp_user("u1", "Tidy up"),
        omp_response("a1", "stop", vec![omp_text("Done for now.")]),
    ];
    let before = read(OMP, &lines);
    assert_eq!(entry(&before, "a1:0").kind, Kind::Message);

    lines.push(omp_response(
        "a2",
        "toolUse",
        vec![
            omp_text("One more thing."),
            omp_call("call_1", "bash", json!({"command": "ls", "i": "Listing"})),
        ],
    ));
    let after = read_since(OMP, &lines, &before.cursor);

    assert_eq!(ids(&after), ["a1:0", "a2:0", "call_1"]);
    assert_eq!(entry(&after, "a1:0").kind, Kind::Note);
    assert_eq!(text(&after, "a1:0"), "Done for now.");
    assert!(!after.turn.finished);
}

// The cursor.

#[test]
fn since_returns_only_entries_added_or_changed_after_the_cursor() {
    let mut lines = vec![
        claude_user("u1", "Run it"),
        claude_text("a1", "m1", Some("tool_use"), "Running."),
        claude_call("a2", "m1", "toolu_1", "Bash", json!({"command": "make"})),
    ];
    let first = read(CLAUDE, &lines);
    assert_eq!(ids(&first), ["u1", "a1:0", "toolu_1"]);

    // Same transcript: nothing to send, and the cursor does not move.
    let idle = read_since(CLAUDE, &lines, &first.cursor);
    assert!(idle.entries.is_empty());
    assert!(!idle.reset);
    assert_eq!(idle.cursor, first.cursor);
    assert!(!idle.turn.finished);

    // The result arrives: the tool comes again, complete; the unchanged note does not.
    lines.push(claude_result("r1", "toolu_1", json!("built"), false));
    let result = read_since(CLAUDE, &lines, &first.cursor);
    assert_eq!(ids(&result), ["toolu_1"]);
    assert_eq!(tool(&result, "toolu_1").status, Status::Succeeded);
    assert_eq!(tool(&result, "toolu_1").output.as_deref(), Some("built"));
    assert_ne!(result.cursor, first.cursor);

    // The final answer arrives.
    lines.push(claude_text("a3", "m2", Some("end_turn"), "Built."));
    let last = read_since(CLAUDE, &lines, &result.cursor);
    assert_eq!(ids(&last), ["a3:0"]);
    assert_eq!(entry(&last, "a3:0").kind, Kind::Message);
    assert!(last.turn.finished);

    // Reading everything again gives the same picture as following along.
    let whole = read(CLAUDE, &lines);
    assert_eq!(whole.cursor, last.cursor);
    assert_eq!(ids(&whole), ["u1", "a1:0", "toolu_1", "a3:0"]);
}

#[test]
fn a_new_prompt_resets_the_cursor_and_returns_the_whole_new_turn() {
    let mut lines = vec![
        claude_user("u1", "First"),
        claude_text("a1", "m1", Some("end_turn"), "One."),
    ];
    let first = read(CLAUDE, &lines);

    lines.push(claude_user("u2", "Second"));
    lines.push(claude_call(
        "a2",
        "m2",
        "toolu_1",
        "Bash",
        json!({"command": "ls"}),
    ));
    let second = read_since(CLAUDE, &lines, &first.cursor);

    assert!(second.reset);
    assert_eq!(ids(&second), ["u2", "toolu_1"]);
    assert_ne!(second.cursor, first.cursor);
    assert!(!second.turn.finished);
}

#[test]
fn a_cursor_that_is_not_from_this_turn_resets() {
    let lines = [
        claude_user("u1", "First"),
        claude_text("a1", "m1", Some("end_turn"), "One."),
    ];
    let first = read(CLAUDE, &lines);
    let parsed = Cursor::decode(&first.cursor).unwrap();
    let future = Cursor {
        end: parsed.end + 1,
        ..parsed
    }
    .encode();
    // The same offsets, another prompt: the transcript was replaced.
    let replaced = [
        claude_user("u2", "First"),
        claude_text("a1", "m1", Some("end_turn"), "One."),
    ];

    for since in [
        "",
        "junk",
        "c:",
        "c:1.2",
        "c:zz.1.2",
        "c:1.2.3.4",
        "c:-1.2.3",
        future.as_str(),
    ] {
        let activity = read_since(CLAUDE, &lines, since);
        assert!(activity.reset, "{since:?} should not apply");
        assert_eq!(ids(&activity), ["u1", "a1:0"], "{since:?}");
    }
    let activity = read_since(CLAUDE, &replaced, &first.cursor);
    assert!(activity.reset);
    assert_eq!(ids(&activity), ["u2", "a1:0"]);
}

#[test]
fn cursor_names_the_same_state_after_a_restart() {
    let cursor = Cursor {
        start: 0x1f2e,
        fingerprint: fingerprint("cursor"),
        end: 0x9a,
    };

    assert_eq!(cursor.encode(), "c:1f2e.f927453fbe6252ef.9a");
    assert_eq!(Cursor::decode(&cursor.encode()), Some(cursor));
}

#[test]
fn limit_keeps_the_newest_entries_and_reports_the_cut() {
    let mut lines = vec![claude_user("u1", "Many steps")];
    for step in 1..=5 {
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
    let text = transcript(&lines);

    let all = read_text(CLAUDE, &text, None, 6).unwrap();
    assert_eq!(all.entries.len(), 6);
    assert!(!all.truncated);

    let newest = read_text(CLAUDE, &text, None, 4).unwrap();
    assert_eq!(ids(&newest), ["toolu_2", "toolu_3", "toolu_4", "toolu_5"]);
    assert!(newest.truncated);
    assert!(!newest.reset);
    // The cursor still covers everything read, so following on works from the end.
    assert_eq!(newest.cursor, all.cursor);
}

/// A running tool finishes and three more calls begin after a cursor was taken: four entries
/// changed. Returns the transcript and that cursor.
fn four_entries_changed_since_a_running_tool() -> (String, String) {
    let mut lines = vec![
        claude_user("u1", "Many steps"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "step 1"})),
    ];
    let cursor = read(CLAUDE, &lines).cursor;
    lines.push(claude_result("r1", "toolu_1", json!("ok"), false));
    for step in 2..=4 {
        lines.push(claude_call(
            &format!("a{step}"),
            &format!("m{step}"),
            &format!("toolu_{step}"),
            "Bash",
            json!({"command": format!("step {step}")}),
        ));
    }
    (transcript(&lines), cursor)
}

#[test]
fn since_with_more_changes_than_the_limit_answers_with_the_newest_entries_of_the_turn() {
    let (text, cursor) = four_entries_changed_since_a_running_tool();

    // Sending only the newest two changes would leave a gap: the first tool, still running on
    // the client, would never be told it finished. The client gets the turn's newest two
    // entries to replace what it holds instead.
    let answer = read_text(CLAUDE, &text, Some(&cursor), 2).unwrap();

    assert!(answer.reset);
    assert_eq!(ids(&answer), ["toolu_3", "toolu_4"]);
    assert!(answer.truncated);
    // The cursor still covers everything read, so following on works from the end.
    let whole = read_text(CLAUDE, &text, None, DEFAULT_LIMIT).unwrap();
    assert_eq!(answer.cursor, whole.cursor);
}

#[test]
fn since_with_exactly_as_many_changes_as_the_limit_stays_incremental() {
    let (text, cursor) = four_entries_changed_since_a_running_tool();

    let answer = read_text(CLAUDE, &text, Some(&cursor), 4).unwrap();

    assert!(!answer.reset && !answer.truncated);
    assert_eq!(ids(&answer), ["toolu_1", "toolu_2", "toolu_3", "toolu_4"]);
    assert_eq!(tool(&answer, "toolu_1").status, Status::Succeeded);
}

#[test]
fn a_line_still_being_written_is_left_for_the_next_read() {
    let head = transcript(&[claude_user("u1", "Go")]);
    let answer = claude_text("a1", "m1", Some("end_turn"), "All done.");
    let next = claude_call("a2", "m2", "toolu_1", "Bash", json!({"command": "ls"}));

    // Half a line is no entry yet.
    let torn = format!("{head}{}", &answer[..answer.len() / 2]);
    let first = read_text(CLAUDE, &torn, None, DEFAULT_LIMIT).unwrap();
    assert_eq!(ids(&first), ["u1"]);
    assert_eq!(
        Cursor::decode(&first.cursor).unwrap().end,
        head.len() as u64
    );

    // Once complete it arrives as new, even with more lines behind it.
    let complete = format!("{head}{answer}\n{next}\n");
    let second = read_text(CLAUDE, &complete, Some(&first.cursor), DEFAULT_LIMIT).unwrap();
    assert!(!second.reset);
    assert_eq!(ids(&second), ["a1:0", "toolu_1"]);

    // A whole line without its newline yet is shown, and offered again once it ends.
    let unterminated = format!("{head}{answer}");
    let shown = read_text(CLAUDE, &unterminated, None, DEFAULT_LIMIT).unwrap();
    assert_eq!(ids(&shown), ["u1", "a1:0"]);
    assert_eq!(
        Cursor::decode(&shown.cursor).unwrap().end,
        head.len() as u64
    );
    let ended = format!("{head}{answer}\n");
    let again = read_text(CLAUDE, &ended, Some(&shown.cursor), DEFAULT_LIMIT).unwrap();
    assert_eq!(ids(&again), ["a1:0"]);
}

#[test]
fn reading_from_disk_follows_a_transcript_as_it_grows() {
    use std::io::Write;

    let dir = std::env::temp_dir().join(format!("herdr-activity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("session.jsonl");
    let mut file = std::fs::File::create(&path).unwrap();
    for line in [
        claude_user("u1", "Go"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "ls"})),
    ] {
        writeln!(file, "{line}").unwrap();
    }

    let first = activity(CLAUDE, &path, None, DEFAULT_LIMIT, false)
        .unwrap()
        .unwrap();
    assert_eq!(ids(&first), ["u1", "toolu_1"]);

    writeln!(
        file,
        "{}",
        claude_result("r1", "toolu_1", json!("x"), false)
    )
    .unwrap();
    let second = activity(CLAUDE, &path, Some(&first.cursor), DEFAULT_LIMIT, false)
        .unwrap()
        .unwrap();
    assert_eq!(ids(&second), ["toolu_1"]);
    assert_eq!(tool(&second, "toolu_1").status, Status::Succeeded);

    let missing = activity(CLAUDE, &dir.join("gone.jsonl"), None, DEFAULT_LIMIT, false);
    assert_eq!(missing.unwrap_err().kind(), io::ErrorKind::NotFound);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn turn_is_found_however_the_file_is_chunked_and_however_large_it_is() {
    let mut lines = vec![claude_user("u0", "Ancient history")];
    for step in 0..200 {
        lines.push(claude_text(
            &format!("h{step}"),
            &format!("mh{step}"),
            Some("end_turn"),
            &"filler ".repeat(50),
        ));
    }
    lines.push(claude_user("u1", "Now"));
    lines.push(claude_text("a1", "m1", Some("end_turn"), "Answer."));
    let text = transcript(&lines);

    for chunk in [7, 64, 1000, READ_CHUNK] {
        let turn = read_turn(CLAUDE, lines_of(&text, chunk), false)
            .unwrap()
            .unwrap();
        let activity = turn.into_activity(None, DEFAULT_LIMIT);
        assert_eq!(ids(&activity), ["u1", "a1:0"], "chunk {chunk}");
    }
}

#[test]
fn lines_with_unpaired_surrogate_escapes_are_still_read() {
    // A result cut in the middle of an emoji leaves `\ud83d` alone. A whole emoji is a pair,
    // and an escaped backslash before `ud83d` is plain text.
    let result = r#"{"type":"user","uuid":"r1","timestamp":"t-r1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"cut \ud83d | pair \ud83d\ude00 | low \ude00 | text \\ud83d"}]}}"#;
    let lines = [
        claude_user("u1", "go"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "run"})),
        result.to_string(),
    ];

    let activity = read(CLAUDE, &lines);

    let call = tool(&activity, "toolu_1");
    assert_eq!(call.status, Status::Succeeded);
    assert_eq!(
        call.output.as_deref(),
        Some("cut \u{fffd} | pair 😀 | low \u{fffd} | text \\ud83d")
    );

    let call = r#"{"type":"message","id":"a1","timestamp":"t-a1","message":{"role":"assistant","stopReason":"toolUse","content":[{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"ls","i":"Listing \ud83d files"}}]}}"#;
    let activity = read(OMP, &[omp_user("u1", "go"), call.to_string()]);

    assert_eq!(tool(&activity, "call_1").summary, "Listing \u{fffd} files");
}

#[test]
fn text_of_a_response_that_turns_out_to_hold_a_tool_call_is_sent_again_block_by_block() {
    let mut lines = vec![
        claude_user("u1", "Look"),
        claude_text("a1", "m1", None, "First."),
        claude_text("a2", "m1", None, "Second."),
    ];
    // Read while the response is still being written, it is one entry holding both blocks.
    let before = read(CLAUDE, &lines);
    assert_eq!(ids(&before), ["u1", "a1:0"]);
    assert_eq!(text(&before, "a1:0"), "First.\n\nSecond.");

    lines.push(claude_call(
        "a3",
        "m1",
        "toolu_1",
        "Bash",
        json!({"command": "ls"}),
    ));
    let after = read_since(CLAUDE, &lines, &before.cursor);

    assert_eq!(ids(&after), ["a1:0", "a2:0", "toolu_1"]);
    assert_eq!(text(&after, "a1:0"), "First.");
    assert_eq!(text(&after, "a2:0"), "Second.");
}

fn omp_custom(
    id: &str,
    custom_type: &str,
    attribution: &str,
    display: bool,
    content: &str,
    details: Value,
) -> String {
    json!({
        "type": "custom_message",
        "id": id,
        "parentId": null,
        "timestamp": format!("t-{id}"),
        "customType": custom_type,
        "attribution": attribution,
        "display": display,
        "content": content,
        "details": details,
    })
    .to_string()
}

#[test]
fn omp_skill_prompt_opens_a_turn_with_what_the_user_typed() {
    let skill = omp_custom(
        "s1",
        "skill-prompt",
        "user",
        true,
        "THE WHOLE EXPANDED SKILL TEXT, PAGES OF IT",
        json!({"name": "review", "args": "the diff", "prompt": "/skill:review the diff"}),
    );
    let lines = [
        omp_user("u0", "Earlier request"),
        omp_response("a0", "stop", vec![omp_text("Earlier answer.")]),
        omp_custom(
            "x1",
            "plan-mode-context",
            "agent",
            false,
            "plan mode",
            json!(null),
        ),
        skill,
        omp_bookkeeping("c1"),
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call(
                "call_1",
                "read",
                json!({"path": "a.rs", "i": "Reading"}),
            )],
        ),
    ];

    let activity = read(OMP, &lines);

    assert_eq!(ids(&activity), ["s1", "call_1"]);
    assert_eq!(text(&activity, "s1"), "/skill:review the diff");
    assert!(!activity.turn.finished);
}

#[test]
fn omp_session_started_by_a_skill_has_a_turn() {
    let lines = [
        omp_custom(
            "s1",
            "skill-prompt",
            "user",
            true,
            "  Do the skill's work.  ",
            json!({"name": "review"}),
        ),
        omp_response("a1", "stop", vec![omp_text("Done.")]),
    ];

    let activity = read(OMP, &lines);

    // Without what the user typed, the skill's own text stands in.
    assert_eq!(text(&activity, "s1"), "Do the skill's work.");
    assert_eq!(kinds(&activity), [Kind::Prompt, Kind::Message]);
    assert!(activity.turn.finished);
}

#[test]
fn omp_notices_and_attachments_do_not_open_turns() {
    let lines = [
        omp_user("u1", "Go"),
        omp_custom(
            "n1",
            "image-attachment",
            "user",
            false,
            "an image",
            json!(null),
        ),
        omp_custom(
            "n2",
            "async-result",
            "agent",
            true,
            "Job finished",
            json!(null),
        ),
        omp_custom("n3", "advisor", "agent", true, "Consider this", json!(null)),
        omp_response("a1", "stop", vec![omp_text("Done.")]),
    ];

    let activity = read(OMP, &lines);

    assert_eq!(ids(&activity), ["u1", "a1:0"]);
}
