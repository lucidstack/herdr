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
    let agent = match super::wait::agent_get(&request_id, &target.target, api_tx) {
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
