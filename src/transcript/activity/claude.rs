//! Claude Code transcripts: one JSON object per line. A response is written as one line per
//! content block, all sharing a message id, and a tool's result is a `user` line of its own.

use serde::Deserialize;
use serde_json::Value;

use super::{
    describe, mcp_kind, message_text, parse_entry, result_tail, Assistant, Block, Call, Line,
    Prompt, Stop, ToolResult,
};
use crate::api::schema::AgentActivityToolKind;

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "isSidechain", default)]
    sidechain: Option<bool>,
    #[serde(rename = "isMeta", default)]
    meta: Option<bool>,
    #[serde(rename = "isCompactSummary", default)]
    compact_summary: Option<bool>,
    #[serde(default)]
    message: Option<Value>,
    /// The text of a `system` line, which has no message.
    #[serde(default)]
    content: Option<Value>,
}

pub(super) fn parse(line: &[u8]) -> Line {
    let Some(entry) = parse_entry::<Entry>(line) else {
        // A line torn by a concurrent write is not an entry.
        return Line::Other;
    };
    // Subagents run in side chains; their work is not the main agent's turn.
    if entry.sidechain.unwrap_or(false) {
        return Line::Other;
    }
    if entry.kind.as_deref() == Some("system") {
        return system(&entry);
    }
    let (Some(uuid), Some(message)) = (entry.uuid, entry.message) else {
        return Line::Other;
    };
    match entry.kind.as_deref() {
        Some("assistant") => assistant(&uuid, entry.timestamp, &message),
        // Meta messages are context Claude Code feeds the model, such as a skill's text.
        Some("user") if !entry.meta.unwrap_or(false) && !entry.compact_summary.unwrap_or(false) => {
            user(uuid, entry.timestamp, &message)
        }
        _ => Line::Other,
    }
}

/// Newer Claude Code versions write what a local slash command printed as a `system` line of
/// its own, where older ones wrote a user message. Every other system line is bookkeeping.
fn system(entry: &Entry) -> Line {
    let printed = entry.subtype.as_deref() == Some("local_command")
        && entry
            .content
            .as_ref()
            .and_then(Value::as_str)
            .is_some_and(is_local_output);
    if printed {
        Line::LocalOutput
    } else {
        Line::Other
    }
}

/// What a local slash command printed, in the tags Claude Code wraps it in.
fn is_local_output(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("<local-command-stdout>") || text.starts_with("<local-command-stderr>")
}

fn assistant(uuid: &str, timestamp: Option<String>, message: &Value) -> Line {
    let stop = match message.get("stop_reason").and_then(Value::as_str) {
        Some("tool_use") => Stop::ToolUse,
        Some(_) => Stop::Ended,
        None => Stop::Unknown,
    };
    let blocks = match message.get("content") {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| block_of(uuid, index, block))
            .collect(),
        Some(Value::String(text)) => text_block(uuid, 0, text).into_iter().collect(),
        _ => Vec::new(),
    };
    Line::Assistant(Assistant {
        response: message
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
        timestamp,
        stop,
        blocks,
    })
}

/// Thinking and every block kind this reader does not know are not part of the feed.
fn block_of(uuid: &str, index: usize, block: &Value) -> Option<Block> {
    match block.get("type")?.as_str()? {
        "text" => text_block(uuid, index, block.get("text")?.as_str()?),
        "tool_use" => {
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Some(Block::Call(Call {
                id: block.get("id")?.as_str()?.to_string(),
                tool: describe(name, kind(name), block.get("input"), None),
            }))
        }
        _ => None,
    }
}

fn text_block(uuid: &str, index: usize, text: &str) -> Option<Block> {
    (!text.trim().is_empty()).then(|| Block::Text {
        id: format!("{uuid}:{index}"),
        text: text.to_string(),
    })
}

fn user(id: String, timestamp: Option<String>, message: &Value) -> Line {
    let content = message.get("content");
    if let Some(Value::Array(blocks)) = content {
        let results: Vec<ToolResult> = blocks.iter().filter_map(tool_result).collect();
        if !results.is_empty() {
            return Line::ToolResults(results);
        }
    }
    match message_text(content) {
        Some(text) => classify(id, timestamp, text),
        None => Line::Other,
    }
}

fn tool_result(block: &Value) -> Option<ToolResult> {
    if block.get("type")?.as_str()? != "tool_result" {
        return None;
    }
    Some(ToolResult {
        call: block.get("tool_use_id")?.as_str()?.to_string(),
        failed: block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        output: block.get("content").and_then(result_tail),
    })
}

/// Claude Code writes more than the user's words as user messages: interruption markers,
/// what local slash commands printed, shell escapes, background task notices.
fn classify(id: String, timestamp: Option<String>, text: String) -> Line {
    if text.starts_with("[Request interrupted by user") {
        return Line::Interrupt;
    }
    if is_local_output(&text) {
        return Line::LocalOutput;
    }
    // The user's own shell commands (`!ls`) and stray reminders never open a turn.
    if [
        "<bash-input>",
        "<bash-stdout>",
        "<bash-stderr>",
        "<system-reminder>",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix))
    {
        return Line::Other;
    }
    let (text, command) = if text.starts_with("<command-") {
        match command_text(&text) {
            Some(command) => (command, true),
            None => (text, false),
        }
    } else if text.starts_with("<task-notification>") {
        // Its summary line says what happened; the rest is plumbing.
        let summary = tag(&text, "summary")
            .map(|summary| summary.trim().to_string())
            .filter(|summary| !summary.is_empty());
        (summary.unwrap_or(text), false)
    } else {
        // A slash command typed as plain text. A prompt that merely starts with a path looks
        // the same; `read_turn` tells them apart by what follows.
        let command = is_slash_command(&text);
        (text, command)
    };
    Line::Prompt(Prompt {
        id,
        timestamp,
        text,
        command,
    })
}

/// `/name` and whatever follows it.
fn is_slash_command(text: &str) -> bool {
    text.strip_prefix('/')
        .and_then(|rest| rest.chars().next())
        .is_some_and(char::is_alphanumeric)
}

/// A slash command as the user typed it: `/name args`.
fn command_text(text: &str) -> Option<String> {
    let name = tag(text, "command-name")?.trim();
    let args = tag(text, "command-args").map(str::trim).unwrap_or_default();
    Some(if args.is_empty() {
        name.to_string()
    } else {
        format!("{name} {args}")
    })
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&format!("</{name}>"))?;
    Some(&text[start..end])
}

fn kind(name: &str) -> AgentActivityToolKind {
    use AgentActivityToolKind::*;
    match name {
        "Bash" | "BashOutput" | "KillShell" | "KillBash" | "Monitor" => Shell,
        "Read" | "NotebookRead" => Read,
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => Edit,
        "Glob" | "Grep" | "LS" | "ToolSearch" => Search,
        "WebFetch" | "WebSearch" => Web,
        "Task" | "Agent" | "TaskOutput" | "TaskStop" | "KillTask" | "SendMessage"
        | "ListAgents" | "SubagentHandback" => Agent,
        "AskUserQuestion" => Question,
        "TodoWrite" | "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskGet" => Todo,
        name => mcp_kind(name),
    }
}
