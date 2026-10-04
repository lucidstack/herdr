//! `agent.last_message`: the socket server resolves the agent through the app, then reads
//! its transcript on the connection's own thread, so a transcript of tens of megabytes
//! never stalls the app loop.

use std::path::Path;

use serde::Serialize;

use crate::api::schema::{
    AgentInfo, AgentLastMessageInfo, AgentLastMessageStatus, AgentTarget, ResponseResult,
    SuccessResponse,
};
use crate::api::ApiRequestSender;
use crate::transcript::TranscriptFormat;

pub(super) fn agent_last_message(
    request_id: String,
    target: AgentTarget,
    api_tx: &ApiRequestSender,
) -> String {
    let agent = match super::wait::agent_for_last_message(&request_id, &target.target, api_tx) {
        Ok(agent) => agent,
        Err(response) => return encode(&response),
    };
    encode(&SuccessResponse {
        id: request_id,
        result: ResponseResult::AgentLastMessage {
            last_message: read_last_message(agent),
        },
    })
}

fn read_last_message(agent: AgentInfo) -> AgentLastMessageInfo {
    let (session_agent, transcript_path) = agent
        .agent_session
        .map(|session| (Some(session.agent), session.transcript_path))
        .unwrap_or_default();
    let mut info = AgentLastMessageInfo {
        terminal_id: agent.terminal_id,
        pane_id: agent.pane_id,
        // The transcript's format follows the agent that owns the session.
        agent: session_agent.or(agent.agent),
        status: AgentLastMessageStatus::NoTranscript,
        text: None,
        replies: Vec::new(),
        stop_reason: None,
        timestamp: None,
        transcript_path,
    };
    let Some(path) = info.transcript_path.as_deref() else {
        return info;
    };
    let Some(format) = info.agent.as_deref().and_then(TranscriptFormat::for_agent) else {
        info.status = AgentLastMessageStatus::UnsupportedFormat;
        return info;
    };
    match crate::transcript::last_message(format, Path::new(path)) {
        Ok(Some(message)) => {
            info.status = AgentLastMessageStatus::Available;
            // Replies count only for the message they were offered for.
            info.replies = agent
                .offered_replies
                .map(|offered| offered.replies_for(&message.text).to_vec())
                .unwrap_or_default();
            info.text = Some(message.text);
            info.stop_reason = message.stop_reason;
            info.timestamp = message.timestamp;
        }
        Ok(None) => info.status = AgentLastMessageStatus::NoMessage,
        Err(error) => {
            tracing::warn!(path, %error, "failed to read agent transcript");
            info.status = AgentLastMessageStatus::Unreadable;
        }
    }
    info
}

pub(super) fn encode(response: &impl Serialize) -> String {
    serde_json::to_string(response).unwrap_or_else(|_| {
        r#"{"id":"","error":{"code":"internal_error","message":"failed to encode response"}}"#
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::*;
    use crate::api::schema::{
        Method, PaneReportAgentSessionParams, PaneReportRepliesParams, Request,
    };
    use crate::app::App;
    use crate::config::Config;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/agent-replies")
            .join(name)
    }

    /// The final message of the recorded Claude Code turn, as its Stop hook reported it.
    fn stop_hook_message() -> String {
        std::fs::read_to_string(fixture("claude-tool-turn.message.txt")).unwrap()
    }

    /// An app with one Claude agent pane whose session reports `transcript`.
    fn app_with_claude_transcript(transcript: &Path) -> (App, String) {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("last-message")];
        app.state.ensure_test_terminals();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.terminal_id_for_pane(0, pane_id).unwrap();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Claude), AgentState::Idle);
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let response = app.handle_api_request(Request {
            id: "session".into(),
            method: Method::PaneReportAgentSession(PaneReportAgentSessionParams {
                pane_id: public_pane_id.clone(),
                source: "herdr:claude".into(),
                agent: "claude".into(),
                seq: Some(1),
                agent_session_id: Some("session-1".into()),
                agent_session_path: Some(transcript.display().to_string()),
                session_start_source: Some("startup".into()),
                resume_argv: None,
            }),
        });
        assert!(serde_json::from_str::<SuccessResponse>(&response).is_ok());
        (app, public_pane_id)
    }

    fn report_replies(app: &mut App, pane_id: &str, message: &str, replies: &[&str]) {
        let response = app.handle_api_request(Request {
            id: "replies".into(),
            method: Method::PaneReportReplies(PaneReportRepliesParams {
                pane_id: pane_id.into(),
                source: "lucidstack.vetch".into(),
                message: message.into(),
                replies: replies.iter().map(|reply| reply.to_string()).collect(),
            }),
        });
        assert!(
            serde_json::from_str::<SuccessResponse>(&response).is_ok(),
            "{response}"
        );
    }

    /// What the socket server answers to `agent.last_message`, with `app` answering what the
    /// server asks it.
    fn last_message_response(app: &mut App, target: &str) -> serde_json::Value {
        let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel();
        let target = target.to_string();
        let server = std::thread::spawn(move || {
            agent_last_message("req".into(), AgentTarget { target }, &api_tx)
        });
        let asked = api_rx
            .blocking_recv()
            .expect("the server asks the app for the agent");
        let answer = app.handle_api_request(asked.request);
        asked.respond_to.send(answer).unwrap();
        serde_json::from_str(&server.join().unwrap()).unwrap()
    }

    fn replies_of(response: &serde_json::Value) -> serde_json::Value {
        response["result"]["last_message"]["replies"].clone()
    }

    #[test]
    fn the_replies_reported_for_the_message_are_returned_with_it() {
        let (mut app, pane_id) = app_with_claude_transcript(&fixture("claude-tool-turn.jsonl"));
        report_replies(&mut app, &pane_id, &stop_hook_message(), &["Red", "Green"]);

        let response = last_message_response(&mut app, &pane_id);

        let last_message = &response["result"]["last_message"];
        assert_eq!(last_message["status"], "available");
        assert!(last_message["text"]
            .as_str()
            .unwrap()
            .starts_with("## Moonbase Lantern System"));
        assert_eq!(last_message["replies"], json!(["Red", "Green"]));
    }

    #[test]
    fn the_replies_are_matched_to_the_message_whatever_the_harness_made_of_its_markup() {
        let (mut app, pane_id) = app_with_claude_transcript(&fixture("claude-tool-turn.jsonl"));
        let flattened = stop_hook_message()
            .replace(['#', '*', '`', '-'], "")
            .replace('\n', "  ");
        report_replies(&mut app, &pane_id, &flattened, &["Red"]);

        let response = last_message_response(&mut app, &pane_id);

        assert_eq!(replies_of(&response), json!(["Red"]));
    }

    #[test]
    fn replies_for_another_message_are_never_returned_but_the_field_is_still_there() {
        let (mut app, pane_id) = app_with_claude_transcript(&fixture("claude-tool-turn.jsonl"));
        report_replies(&mut app, &pane_id, "Should I rebase the branch?", &["Yes"]);

        let response = last_message_response(&mut app, &pane_id);

        assert_eq!(response["result"]["last_message"]["status"], "available");
        assert_eq!(replies_of(&response), json!([]));
    }

    #[test]
    fn replies_stop_applying_when_the_agent_finishes_a_newer_message() {
        let transcript = std::env::temp_dir().join(format!(
            "herdr-last-message-newer-{}.jsonl",
            std::process::id()
        ));
        std::fs::copy(fixture("claude-tool-turn.jsonl"), &transcript).unwrap();
        let (mut app, pane_id) = app_with_claude_transcript(&transcript);
        report_replies(&mut app, &pane_id, &stop_hook_message(), &["Red"]);
        assert_eq!(
            replies_of(&last_message_response(&mut app, &pane_id)),
            json!(["Red"])
        );

        // The next turn ends. Its message is on disk before anything is reported for it.
        let newer = json!({
            "parentUuid": "9ffff385-6367-45dc-b8af-262dabe3bde5",
            "isSidechain": false,
            "type": "assistant",
            "uuid": "00000000-0000-4000-8000-000000000001",
            "timestamp": "2026-10-04T19:45:00.000Z",
            "message": {
                "id": "msg_newer",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "Both lamps are tagged. Anything else?"}],
                "stop_reason": "end_turn",
            },
        });
        let mut content = std::fs::read_to_string(&transcript).unwrap();
        content.push_str(&format!("{newer}\n"));
        std::fs::write(&transcript, content).unwrap();

        let response = last_message_response(&mut app, &pane_id);
        assert_eq!(
            response["result"]["last_message"]["text"],
            "Both lamps are tagged. Anything else?"
        );
        assert_eq!(replies_of(&response), json!([]));

        report_replies(
            &mut app,
            &pane_id,
            "Both lamps are tagged. Anything else?",
            &["No"],
        );
        assert_eq!(
            replies_of(&last_message_response(&mut app, &pane_id)),
            json!(["No"])
        );
        let _ = std::fs::remove_file(&transcript);
    }

    #[test]
    fn no_replies_are_returned_when_the_message_cannot_be_read() {
        let missing = std::env::temp_dir().join("herdr-last-message-missing.jsonl");
        let (mut app, pane_id) = app_with_claude_transcript(&missing);
        report_replies(&mut app, &pane_id, &stop_hook_message(), &["Red"]);

        let response = last_message_response(&mut app, &pane_id);

        assert_eq!(response["result"]["last_message"]["status"], "unreadable");
        assert_eq!(replies_of(&response), json!([]));
    }
}
