//! `running_shell_calls`: the shell commands of the current turn that have no result yet.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::super::running::{read_running, running_shell_calls, RunningShellCall};
use super::*;

/// The line as the agents record one written `seconds` and `millis` after the Unix epoch.
fn written_at(line: String, seconds: u64, millis: u32) -> String {
    let nanos = i128::from(seconds) * 1_000_000_000 + i128::from(millis) * 1_000_000;
    let at = OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap();
    let mut value: Value = serde_json::from_str(&line).unwrap();
    value["timestamp"] = json!(at.format(&Rfc3339).unwrap());
    value.to_string()
}

fn without_timestamp(line: String) -> String {
    let mut value: Value = serde_json::from_str(&line).unwrap();
    value.as_object_mut().unwrap().remove("timestamp");
    value.to_string()
}

fn moment(seconds: u64, millis: u32) -> Option<SystemTime> {
    Some(UNIX_EPOCH + Duration::new(seconds, millis * 1_000_000))
}

fn running(format: TranscriptFormat, lines: &[String]) -> Vec<RunningShellCall> {
    read_running(format, lines_of(&transcript(lines), READ_CHUNK), u64::MAX).unwrap()
}

fn call(id: &str, started_at: Option<SystemTime>, label: &str) -> RunningShellCall {
    RunningShellCall {
        call_id: id.into(),
        started_at,
        label: label.into(),
    }
}

fn call_ids(calls: &[RunningShellCall]) -> Vec<&str> {
    calls.iter().map(|call| call.call_id.as_str()).collect()
}

#[test]
fn a_claude_command_without_a_result_runs_since_its_line_was_written() {
    let lines = [
        claude_user("u1", "Run the tests"),
        written_at(
            claude_call(
                "a1",
                "m1",
                "toolu_1",
                "Bash",
                json!({"command": "cargo test --all\nsecond line", "description": "Run tests"}),
            ),
            1_760_000_000,
            488,
        ),
    ];
    assert_eq!(
        running(CLAUDE, &lines),
        [call(
            "toolu_1",
            moment(1_760_000_000, 488),
            "cargo test --all"
        )]
    );
}

#[test]
fn an_omp_command_without_a_result_runs_since_its_message_was_written() {
    let lines = [
        omp_user("u1", "Run the tests"),
        written_at(
            omp_response(
                "a1",
                "toolUse",
                vec![omp_call(
                    "call_1",
                    "bash",
                    json!({"command": "sleep 900", "i": "Waiting for the build"}),
                )],
            ),
            1_760_000_000,
            0,
        ),
    ];
    assert_eq!(
        running(OMP, &lines),
        [call("call_1", moment(1_760_000_000, 0), "sleep 900")]
    );
}

#[test]
fn a_command_whose_result_came_is_no_longer_running_whether_it_failed_or_not() {
    let claude = [
        claude_user("u1", "go"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "ls"})),
        claude_result("r1", "toolu_1", json!("boom"), true),
    ];
    assert_eq!(running(CLAUDE, &claude), []);

    let omp = [
        omp_user("u1", "go"),
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call("call_1", "bash", json!({"command": "ls"}))],
        ),
        omp_result("r1", "call_1", "ok", false),
    ];
    assert_eq!(running(OMP, &omp), []);
}

#[test]
fn only_the_calls_of_a_shell_tool_are_commands() {
    let claude = [
        claude_user("u1", "go"),
        claude_call("a1", "m1", "toolu_read", "Read", json!({"file_path": "/a"})),
        claude_call(
            "a2",
            "m2",
            "toolu_task",
            "Task",
            json!({"description": "look"}),
        ),
        claude_call("a3", "m3", "toolu_bash", "Bash", json!({"command": "ls"})),
    ];
    assert_eq!(call_ids(&running(CLAUDE, &claude)), ["toolu_bash"]);

    let omp = [
        omp_user("u1", "go"),
        omp_response(
            "a1",
            "toolUse",
            vec![
                omp_call("call_read", "read", json!({"path": "/a"})),
                omp_call("call_task", "task", json!({"i": "Looking around"})),
                omp_call("call_bash", "bash", json!({"command": "ls"})),
            ],
        ),
    ];
    assert_eq!(call_ids(&running(OMP, &omp)), ["call_bash"]);
}

#[test]
fn of_parallel_commands_those_without_a_result_are_running_in_the_order_they_began() {
    let claude = [
        claude_user("u1", "go"),
        claude_call("a1", "m1", "toolu_a", "Bash", json!({"command": "one"})),
        claude_call("a2", "m1", "toolu_b", "Bash", json!({"command": "two"})),
        claude_call("a3", "m1", "toolu_c", "Bash", json!({"command": "three"})),
        claude_result("r1", "toolu_b", json!("done"), false),
    ];
    assert_eq!(call_ids(&running(CLAUDE, &claude)), ["toolu_a", "toolu_c"]);

    let omp = [
        omp_user("u1", "go"),
        omp_response(
            "a1",
            "toolUse",
            vec![
                omp_call("call_a", "bash", json!({"command": "one"})),
                omp_call("call_b", "bash", json!({"command": "two"})),
                omp_call("call_c", "bash", json!({"command": "three"})),
            ],
        ),
        omp_result("r1", "call_b", "done", false),
    ];
    assert_eq!(call_ids(&running(OMP, &omp)), ["call_a", "call_c"]);
}

#[test]
fn a_command_of_an_earlier_turn_is_not_running() {
    let claude = [
        claude_user("u1", "first"),
        claude_call("a1", "m1", "toolu_old", "Bash", json!({"command": "old"})),
        claude_user("u2", "second"),
        claude_call("a2", "m2", "toolu_new", "Bash", json!({"command": "new"})),
    ];
    assert_eq!(call_ids(&running(CLAUDE, &claude)), ["toolu_new"]);

    let omp = [
        omp_user("u1", "first"),
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call("call_old", "bash", json!({"command": "old"}))],
        ),
        omp_user("u2", "second"),
        omp_response(
            "a2",
            "toolUse",
            vec![omp_call("call_new", "bash", json!({"command": "new"}))],
        ),
    ];
    assert_eq!(call_ids(&running(OMP, &omp)), ["call_new"]);
}

#[test]
fn a_command_of_a_subagent_is_not_the_agents_own() {
    let lines = [
        claude_user("u1", "go"),
        claude_flagged(
            claude_call("a1", "m1", "toolu_side", "Bash", json!({"command": "ls"})),
            "isSidechain",
        ),
    ];
    assert_eq!(running(CLAUDE, &lines), []);
}

#[test]
fn a_call_is_running_with_no_start_when_its_line_has_no_time_that_can_be_read() {
    // The fixtures' own timestamps, `t-a1`, are not times.
    let unreadable = [
        claude_user("u1", "go"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "ls"})),
    ];
    assert_eq!(running(CLAUDE, &unreadable), [call("toolu_1", None, "ls")]);

    let missing = [
        omp_user("u1", "go"),
        without_timestamp(omp_response(
            "a1",
            "toolUse",
            vec![omp_call("call_1", "bash", json!({"command": "ls"}))],
        )),
    ];
    assert_eq!(running(OMP, &missing), [call("call_1", None, "ls")]);
}

#[test]
fn a_command_with_no_command_text_is_labelled_by_what_the_call_says_it_does() {
    let lines = [
        omp_user("u1", "go"),
        omp_response(
            "a1",
            "toolUse",
            vec![omp_call(
                "call_1",
                "eval",
                json!({"i": "Checking the worktree"}),
            )],
        ),
    ];
    assert_eq!(
        running(OMP, &lines),
        [call("call_1", None, "Checking the worktree")]
    );
}

#[test]
fn a_line_still_being_written_does_not_hide_the_command_before_it() {
    let head = transcript(&[
        claude_user("u1", "go"),
        claude_call("a1", "m1", "toolu_1", "Bash", json!({"command": "ls"})),
    ]);
    let torn = format!("{head}{{\"type\":\"user\",\"uuid\":\"r1\",\"timest");
    let calls = read_running(CLAUDE, lines_of(&torn, READ_CHUNK), u64::MAX).unwrap();
    assert_eq!(call_ids(&calls), ["toolu_1"]);
}

#[test]
fn a_command_that_began_more_than_four_mebibytes_ago_is_not_found() {
    let mut lines = vec![
        claude_user("u1", "go"),
        claude_call("a1", "m1", "toolu_old", "Bash", json!({"command": "old"})),
    ];
    // Well over 4 MiB of work after the old command, with a newer one at the end.
    for n in 0..5000 {
        lines.push(claude_text(
            &format!("t{n}"),
            &format!("n{n}"),
            Some("end_turn"),
            &"x".repeat(1000),
        ));
    }
    lines.push(claude_call(
        "a2",
        "m2",
        "toolu_new",
        "Bash",
        json!({"command": "new"}),
    ));
    let text = transcript(&lines);
    assert!(text.len() > 5 * 1000 * 1000);

    // Without the cap both are found: it is the cap that hides the old one.
    let uncapped = read_running(CLAUDE, lines_of(&text, READ_CHUNK), u64::MAX).unwrap();
    assert_eq!(call_ids(&uncapped), ["toolu_old", "toolu_new"]);

    let dir = std::env::temp_dir().join(format!("herdr-running-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.jsonl");
    std::fs::write(&path, &text).unwrap();
    let calls = running_shell_calls(CLAUDE, &path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(call_ids(&calls), ["toolu_new"]);
}

#[test]
fn a_transcript_that_cannot_be_opened_is_an_error() {
    let missing = std::env::temp_dir().join("herdr-running-missing.jsonl");
    assert_eq!(
        running_shell_calls(CLAUDE, &missing).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}
