//! `agent.activity`: like `agent.last_message`, the socket server resolves the agent through
//! the app, then reads its transcript on the connection's own thread, so a transcript of
//! tens of megabytes never stalls the app loop.

use std::path::Path;

use crate::api::schema::{
    AgentActivityInfo, AgentActivityParams, AgentActivityStatus, AgentInfo, ErrorBody,
    ErrorResponse, ResponseResult, SuccessResponse,
};
use crate::api::ApiRequestSender;
use crate::transcript::{TranscriptFormat, DEFAULT_LIMIT, MAX_LIMIT};

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
            activity: read_activity(agent, params.since.as_deref(), limit),
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

fn read_activity(agent: AgentInfo, since: Option<&str>, limit: usize) -> AgentActivityInfo {
    let (session_agent, transcript_path) = agent
        .agent_session
        .map(|session| (Some(session.agent), session.transcript_path))
        .unwrap_or_default();
    let mut info = AgentActivityInfo {
        terminal_id: agent.terminal_id,
        pane_id: agent.pane_id,
        // The transcript's format follows the agent that owns the session.
        agent: session_agent.or(agent.agent),
        status: AgentActivityStatus::NoTranscript,
        turn: None,
        entries: Vec::new(),
        cursor: None,
        reset: false,
        truncated: false,
        transcript_path,
    };
    let Some(path) = info.transcript_path.as_deref() else {
        return info;
    };
    let Some(format) = info.agent.as_deref().and_then(TranscriptFormat::for_agent) else {
        info.status = AgentActivityStatus::UnsupportedFormat;
        return info;
    };
    match crate::transcript::activity(format, Path::new(path), since, limit) {
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
            let info = read_activity(agent, None, 200);

            assert_eq!(info.status, AgentActivityStatus::NoTranscript);
            assert!(info.entries.is_empty());
            assert_eq!((info.turn, info.cursor), (None, None));
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
        );
        assert_eq!(info.status, AgentActivityStatus::Unreadable);

        let empty = transcript_file("empty.jsonl", "");
        let info = read_activity(
            agent_info(Some("claude"), Some(("claude", empty.to_str()))),
            None,
            200,
        );
        assert_eq!(info.status, AgentActivityStatus::NoActivity);
        assert!(info.turn.is_none() && info.cursor.is_none());
    }

    #[test]
    fn readable_transcript_reports_its_current_turn_under_the_sessions_agent() {
        let path = transcript_file("claude.jsonl", &claude_prompt_and_answer());
        // The pane may still be labelled as another agent; the session decides the format.
        let agent = agent_info(Some("omp"), Some(("claude", path.to_str())));

        let info = read_activity(agent.clone(), None, 200);

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
        assert!(!info.reset && !info.truncated);

        // Handing the cursor back returns only what changed: nothing.
        let again = read_activity(agent, info.cursor.as_deref(), 200);
        assert!(again.entries.is_empty());
        assert_eq!(again.cursor, info.cursor);
        assert!(!again.reset);
    }
}
