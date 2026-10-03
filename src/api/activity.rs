//! `agent.activity` and `agent.history`: like `agent.last_message`, the socket server resolves
//! the agent through the app, then reads its transcript on the connection's own thread, so a
//! transcript of tens of megabytes never stalls the app loop.

use std::path::Path;

use crate::api::schema::{
    AgentActivityInfo, AgentActivityParams, AgentActivityStatus, AgentHistoryInfo,
    AgentHistoryParams, AgentInfo, ErrorBody, ErrorResponse, ResponseResult, SuccessResponse,
};
use crate::api::ApiRequestSender;
use crate::transcript::{TranscriptFormat, DEFAULT_LIMIT, DEFAULT_TURNS, MAX_LIMIT, MAX_TURNS};

use super::last_message::encode;

pub(super) fn agent_activity(
    request_id: String,
    params: AgentActivityParams,
    api_tx: &ApiRequestSender,
) -> String {
    let Some(limit) = effective_limit(params.limit) else {
        return encode(&ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "invalid_params".into(),
                message: format!("limit must be between 1 and {MAX_LIMIT}"),
            },
        });
    };
    let agent = match super::wait::agent_get(&request_id, &params.target, api_tx) {
        Ok(agent) => agent,
        Err(response) => return encode(&response),
    };
    encode(&SuccessResponse {
        id: request_id,
        result: ResponseResult::AgentActivity {
            activity: read_activity(
                agent,
                params.since.as_deref(),
                limit,
                params.include_thinking,
            ),
        },
    })
}

pub(super) fn agent_history(
    request_id: String,
    params: AgentHistoryParams,
    api_tx: &ApiRequestSender,
) -> String {
    let Some(turns) = effective_turns(params.turns) else {
        return encode(&ErrorResponse {
            id: request_id,
            error: ErrorBody {
                code: "invalid_params".into(),
                message: format!("turns must be between 1 and {MAX_TURNS}"),
            },
        });
    };
    let agent = match super::wait::agent_get(&request_id, &params.target, api_tx) {
        Ok(agent) => agent,
        Err(response) => return encode(&response),
    };
    encode(&SuccessResponse {
        id: request_id,
        result: ResponseResult::AgentHistory {
            history: read_history(
                agent,
                params.before.as_deref(),
                turns,
                params.include_thinking,
            ),
        },
    })
}

/// The number of entries to return, or `None` when the request's limit is out of range.
fn effective_limit(limit: Option<u32>) -> Option<usize> {
    match limit {
        None => Some(DEFAULT_LIMIT),
        Some(limit) if (1..=MAX_LIMIT as u32).contains(&limit) => Some(limit as usize),
        Some(_) => None,
    }
}

/// The number of turns to return, or `None` when the request's count is out of range.
fn effective_turns(turns: Option<u32>) -> Option<usize> {
    match turns {
        None => Some(DEFAULT_TURNS),
        Some(turns) if (1..=MAX_TURNS as u32).contains(&turns) => Some(turns as usize),
        Some(_) => None,
    }
}

/// The agent that owns the session, whose format the transcript has even when the pane is
/// labelled as another agent, and where the session's transcript is.
fn session_of(agent: &AgentInfo) -> (Option<String>, Option<String>) {
    let session = agent.agent_session.as_ref();
    (
        session
            .map(|session| session.agent.clone())
            .or_else(|| agent.agent.clone()),
        session.and_then(|session| session.transcript_path.clone()),
    )
}

/// Where to read the transcript and in what format, or why there is nothing to read.
fn reader<'a>(
    path: Option<&'a str>,
    agent: Option<&str>,
) -> Result<(&'a str, TranscriptFormat), AgentActivityStatus> {
    let path = path.ok_or(AgentActivityStatus::NoTranscript)?;
    let format = agent
        .and_then(TranscriptFormat::for_agent)
        .ok_or(AgentActivityStatus::UnsupportedFormat)?;
    Ok((path, format))
}

fn read_activity(
    agent: AgentInfo,
    since: Option<&str>,
    limit: usize,
    thinking: bool,
) -> AgentActivityInfo {
    let (session_agent, transcript_path) = session_of(&agent);
    let mut info = AgentActivityInfo {
        terminal_id: agent.terminal_id,
        pane_id: agent.pane_id,
        agent: session_agent,
        status: AgentActivityStatus::NoTranscript,
        turn: None,
        entries: Vec::new(),
        cursor: None,
        reset: false,
        truncated: false,
        transcript_path,
    };
    let (path, format) = match reader(info.transcript_path.as_deref(), info.agent.as_deref()) {
        Ok(reader) => reader,
        Err(status) => {
            info.status = status;
            return info;
        }
    };
    match crate::transcript::activity(format, Path::new(path), since, limit, thinking) {
        Ok(Some(activity)) => {
            info.status = AgentActivityStatus::Available;
            info.turn = Some(activity.turn);
            info.entries = activity.entries;
            info.cursor = Some(activity.cursor);
            info.reset = activity.reset;
            info.truncated = activity.truncated;
        }
        Ok(None) => info.status = AgentActivityStatus::NoActivity,
        Err(error) => {
            tracing::warn!(path, %error, "failed to read agent transcript");
            info.status = AgentActivityStatus::Unreadable;
        }
    }
    info
}

fn read_history(
    agent: AgentInfo,
    before: Option<&str>,
    turns: usize,
    thinking: bool,
) -> AgentHistoryInfo {
    let (session_agent, transcript_path) = session_of(&agent);
    let mut info = AgentHistoryInfo {
        terminal_id: agent.terminal_id,
        pane_id: agent.pane_id,
        agent: session_agent,
        status: AgentActivityStatus::NoTranscript,
        turns: Vec::new(),
        before: None,
        reset: false,
        transcript_path,
    };
    let (path, format) = match reader(info.transcript_path.as_deref(), info.agent.as_deref()) {
        Ok(reader) => reader,
        Err(status) => {
            info.status = status;
            return info;
        }
    };
    match crate::transcript::history(format, Path::new(path), before, turns, thinking) {
        Ok(Some(history)) => {
            info.status = AgentActivityStatus::Available;
            info.turns = history.turns;
            info.before = history.before;
            info.reset = history.reset;
        }
        // Without a prompt there is nothing a cursor could name, so one that was given did
        // not apply.
        Ok(None) => {
            info.status = AgentActivityStatus::NoActivity;
            info.reset = before.is_some();
        }
        Err(error) => {
            tracing::warn!(path, %error, "failed to read agent transcript");
            info.status = AgentActivityStatus::Unreadable;
        }
    }
    info
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::api::schema::AgentActivityEntryKind;

    fn agent_info(agent: Option<&str>, session: Option<(&str, Option<&str>)>) -> AgentInfo {
        let mut value = json!({
            "terminal_id": "term_1",
            "agent_status": "idle",
            "workspace_id": "w1",
            "tab_id": "w1:t1",
            "pane_id": "w1:p1",
            "focused": false,
            "revision": 1,
        });
        if let Some(agent) = agent {
            value["agent"] = json!(agent);
        }
        if let Some((agent, transcript_path)) = session {
            value["agent_session"] = json!({
                "source": format!("herdr:{agent}"),
                "agent": agent,
                "kind": "id",
                "value": "session-1",
            });
            if let Some(path) = transcript_path {
                value["agent_session"]["transcript_path"] = json!(path);
            }
        }
        serde_json::from_value(value).unwrap()
    }

    fn transcript_file(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("herdr-api-activity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn claude_prompt_and_answer() -> String {
        [
            json!({"type": "user", "uuid": "u1", "timestamp": "t1",
                "message": {"role": "user", "content": "Say hello"}}),
            json!({"type": "assistant", "uuid": "a1", "timestamp": "t2",
                "message": {"id": "m1", "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "Hello."}]}}),
        ]
        .iter()
        .map(|line| format!("{line}\n"))
        .collect()
    }

    #[test]
    fn limit_defaults_to_two_hundred_and_stays_between_one_and_five_hundred() {
        assert_eq!(effective_limit(None), Some(200));
        assert_eq!(effective_limit(Some(1)), Some(1));
        assert_eq!(effective_limit(Some(500)), Some(500));
        assert_eq!(effective_limit(Some(0)), None);
        assert_eq!(effective_limit(Some(501)), None);
        assert_eq!(effective_limit(Some(u32::MAX)), None);
    }

    #[test]
    fn out_of_range_limit_is_rejected_before_the_agent_is_looked_up() {
        let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
        for limit in [0, 501] {
            let response = agent_activity(
                "req_1".into(),
                AgentActivityParams {
                    target: "w1:p1".into(),
                    since: None,
                    limit: Some(limit),
                    include_thinking: false,
                },
                &api_tx,
            );

            let error: ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(error.id, "req_1");
            assert_eq!(error.error.code, "invalid_params");
        }
        assert!(api_rx.try_recv().is_err(), "the app was never asked");
    }

    #[test]
    fn agent_without_a_reported_transcript_has_none_to_read() {
        for agent in [
            agent_info(Some("claude"), None),
            agent_info(Some("claude"), Some(("claude", None))),
        ] {
            let info = read_activity(agent, None, 200, false);

            assert_eq!(info.status, AgentActivityStatus::NoTranscript);
            assert_eq!(info.pane_id, "w1:p1");
        }
    }

    #[test]
    fn agent_without_a_reader_is_unsupported() {
        let path = transcript_file("codex.jsonl", &claude_prompt_and_answer());

        let info = read_activity(
            agent_info(Some("codex"), Some(("codex", path.to_str()))),
            None,
            200,
            false,
        );

        assert_eq!(info.status, AgentActivityStatus::UnsupportedFormat);
        assert!(info.entries.is_empty());
        assert_eq!(info.agent.as_deref(), Some("codex"));
    }

    #[test]
    fn missing_and_empty_transcripts_are_told_apart() {
        let missing = std::env::temp_dir().join("herdr-api-activity-missing.jsonl");
        let info = read_activity(
            agent_info(Some("claude"), Some(("claude", missing.to_str()))),
            None,
            200,
            false,
        );
        assert_eq!(info.status, AgentActivityStatus::Unreadable);

        let empty = transcript_file("empty.jsonl", "");
        let info = read_activity(
            agent_info(Some("claude"), Some(("claude", empty.to_str()))),
            None,
            200,
            false,
        );
        assert_eq!(info.status, AgentActivityStatus::NoActivity);
    }

    #[test]
    fn readable_transcript_reports_its_current_turn_under_the_sessions_agent() {
        let path = transcript_file("claude.jsonl", &claude_prompt_and_answer());
        // The pane may still be labelled as another agent; the session decides the format.
        let agent = agent_info(Some("omp"), Some(("claude", path.to_str())));

        let info = read_activity(agent.clone(), None, 200, false);

        assert_eq!(info.status, AgentActivityStatus::Available);
        assert_eq!(info.agent.as_deref(), Some("claude"));
        assert_eq!(info.terminal_id, "term_1");
        assert_eq!(info.transcript_path.as_deref(), path.to_str());
        let turn = info.turn.expect("a turn");
        assert!(turn.finished);
        assert_eq!(turn.started_at.as_deref(), Some("t1"));
        let kinds: Vec<_> = info.entries.iter().map(|entry| entry.kind).collect();
        assert_eq!(
            kinds,
            [
                AgentActivityEntryKind::Prompt,
                AgentActivityEntryKind::Message
            ]
        );

        // Handing the cursor back returns only what changed: nothing.
        let again = read_activity(agent, info.cursor.as_deref(), 200, false);
        assert!(again.entries.is_empty());
        assert_eq!(again.cursor, info.cursor);
        assert!(!again.reset);
    }

    /// Two turns of a Claude Code conversation, linked the way Claude Code links them.
    fn claude_two_turns() -> String {
        [
            json!({"type": "user", "uuid": "u1", "parentUuid": null, "timestamp": "t1",
                "message": {"role": "user", "content": "Say hello"}}),
            json!({"type": "assistant", "uuid": "a1", "parentUuid": "u1", "timestamp": "t2",
                "message": {"id": "m1", "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "Hello."}]}}),
            json!({"type": "user", "uuid": "u2", "parentUuid": "a1", "timestamp": "t3",
                "message": {"role": "user", "content": "Say goodbye"}}),
            json!({"type": "assistant", "uuid": "a2", "parentUuid": "u2", "timestamp": "t4",
                "message": {"id": "m2", "stop_reason": "end_turn",
                    "content": [{"type": "text", "text": "Goodbye."}]}}),
        ]
        .iter()
        .map(|line| format!("{line}\n"))
        .collect()
    }

    #[test]
    fn turns_default_to_five_and_stay_between_one_and_twenty() {
        assert_eq!(effective_turns(None), Some(5));
        assert_eq!(effective_turns(Some(1)), Some(1));
        assert_eq!(effective_turns(Some(20)), Some(20));
        assert_eq!(effective_turns(Some(0)), None);
        assert_eq!(effective_turns(Some(21)), None);
        assert_eq!(effective_turns(Some(u32::MAX)), None);
    }

    #[test]
    fn out_of_range_turns_are_rejected_before_the_agent_is_looked_up() {
        let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
        for turns in [0, 21] {
            let response = agent_history(
                "req_1".into(),
                AgentHistoryParams {
                    target: "w1:p1".into(),
                    before: None,
                    turns: Some(turns),
                    include_thinking: false,
                },
                &api_tx,
            );

            let error: ErrorResponse = serde_json::from_str(&response).unwrap();
            assert_eq!(error.id, "req_1");
            assert_eq!(error.error.code, "invalid_params");
        }
        assert!(api_rx.try_recv().is_err(), "the app was never asked");
    }

    #[test]
    fn history_says_why_there_is_nothing_to_read() {
        let codex = transcript_file("history-codex.jsonl", &claude_two_turns());
        let missing = std::env::temp_dir().join("herdr-api-history-missing.jsonl");
        let empty = transcript_file("history-empty.jsonl", "");
        let cases = [
            (
                agent_info(Some("claude"), None),
                AgentActivityStatus::NoTranscript,
            ),
            (
                agent_info(Some("codex"), Some(("codex", codex.to_str()))),
                AgentActivityStatus::UnsupportedFormat,
            ),
            (
                agent_info(Some("claude"), Some(("claude", missing.to_str()))),
                AgentActivityStatus::Unreadable,
            ),
            (
                agent_info(Some("claude"), Some(("claude", empty.to_str()))),
                AgentActivityStatus::NoActivity,
            ),
        ];

        for (agent, status) in cases {
            let info = read_history(agent, None, 5, false);

            assert_eq!(info.status, status);
        }
    }

    #[test]
    fn history_cursor_for_a_transcript_with_no_prompt_is_a_reset() {
        let empty = transcript_file("history-empty-reset.jsonl", "");
        let agent = agent_info(Some("claude"), Some(("claude", empty.to_str())));

        let info = read_history(agent, Some("h:0.0"), 5, false);

        assert_eq!(info.status, AgentActivityStatus::NoActivity);
        assert!(info.reset, "the client holds turns this transcript lacks");
    }

    #[test]
    fn the_sessions_agent_decides_the_format_history_is_read_in() {
        let path = transcript_file("history-claude.jsonl", &claude_two_turns());
        // The pane may still be labelled as another agent; the session decides the format.
        let agent = agent_info(Some("omp"), Some(("claude", path.to_str())));

        let info = read_history(agent, None, 5, false);

        assert_eq!(info.status, AgentActivityStatus::Available);
        assert_eq!(info.agent.as_deref(), Some("claude"));
        let ids: Vec<_> = info.turns.iter().map(|turn| turn.id.as_str()).collect();
        assert_eq!(ids, ["u1", "u2"]);
    }
}
