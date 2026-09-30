//! omp transcripts: one JSON object per line, forming a tree by `id` and `parentId`. A
//! `message` line holds a whole response or a whole tool result. A prompt the user sent
//! through a skill or a collaborator is a `custom_message` line instead of a user message.

use serde::Deserialize;
use serde_json::Value;

use super::{
    describe, first_line, mcp_kind, message_text, parse_entry, result_tail, shorten, Assistant,
    Block, Call, Keep, Line, Prompt, Stop, ToolResult, TARGET_CHARS,
};
use crate::api::schema::AgentActivityToolKind;

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    message: Option<Value>,
    // The fields of a `custom_message` line.
    #[serde(default)]
    attribution: Option<String>,
    #[serde(default)]
    display: Option<bool>,
    #[serde(default)]
    content: Option<Value>,
    #[serde(default)]
    details: Option<Value>,
}

pub(super) fn parse(line: &[u8]) -> Line {
    let Some(entry) = parse_entry::<Entry>(line) else {
        // A line torn by a concurrent write is not an entry.
        return Line::Other;
    };
    match entry.kind.as_deref() {
        Some("message") => {}
        Some("custom_message") => return custom_prompt(entry),
        _ => return Line::Other,
    }
    let (Some(id), Some(message)) = (entry.id, entry.message) else {
        return Line::Other;
    };
    match message.get("role").and_then(Value::as_str) {
        Some("user") => match message_text(message.get("content")) {
            Some(text) => Line::Prompt(Prompt {
                id,
                timestamp: entry.timestamp,
                text,
                command: false,
            }),
            None => Line::Other,
        },
        Some("assistant") => assistant(&id, entry.timestamp, &message),
        Some("toolResult") => tool_result(&message),
        _ => Line::Other,
    }
}

/// A prompt that reaches omp as a custom message: a skill the user invoked, a message from a
/// collaborator. What the user sees is what counts. Notices the agent sends itself, and
/// attachments that have no display, are not prompts.
fn custom_prompt(entry: Entry) -> Line {
    if entry.attribution.as_deref() != Some("user") || entry.display != Some(true) {
        return Line::Other;
    }
    let Some(id) = entry.id else {
        return Line::Other;
    };
    // A skill's content is the whole skill text; `details.prompt` is what the user typed.
    let typed = entry
        .details
        .as_ref()
        .and_then(|details| details.get("prompt"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
        .map(str::to_string);
    match typed.or_else(|| message_text(entry.content.as_ref())) {
        Some(text) => Line::Prompt(Prompt {
            id,
            timestamp: entry.timestamp,
            text,
            command: false,
        }),
        None => Line::Other,
    }
}

fn assistant(id: &str, timestamp: Option<String>, message: &Value) -> Line {
    let stop = match message.get("stopReason").and_then(Value::as_str) {
        Some("toolUse") => Stop::ToolUse,
        Some(_) => Stop::Ended,
        None => Stop::Unknown,
    };
    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .enumerate()
                .filter_map(|(index, block)| block_of(id, index, block))
                .collect()
        })
        .unwrap_or_default();
    Line::Assistant(Assistant {
        response: None,
        timestamp,
        stop,
        blocks,
    })
}

/// Thinking and every block kind this reader does not know are not part of the feed.
fn block_of(id: &str, index: usize, block: &Value) -> Option<Block> {
    match block.get("type")?.as_str()? {
        "text" => {
            let text = block.get("text")?.as_str()?;
            (!text.trim().is_empty()).then(|| Block::Text {
                id: format!("{id}:{index}"),
                text: text.to_string(),
            })
        }
        "toolCall" => {
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let input = block.get("arguments");
            // Every call carries what the agent meant to do with it.
            let intent = input
                .and_then(|input| input.get("i"))
                .and_then(Value::as_str)
                .or_else(|| block.get("intent").and_then(Value::as_str));
            let mut tool = describe(name, kind(name), input, intent);
            if tool.kind == AgentActivityToolKind::Edit && tool.target.is_none() {
                tool.target = input
                    .and_then(|input| input.get("input"))
                    .and_then(Value::as_str)
                    .and_then(patch_path)
                    .map(|path| shorten(path, TARGET_CHARS, Keep::Tail));
            }
            Some(Block::Call(Call {
                id: block.get("id")?.as_str()?.to_string(),
                tool,
            }))
        }
        _ => None,
    }
}

/// omp's edit tool takes one patch whose `[path#tag]` header names the file it changes.
fn patch_path(patch: &str) -> Option<&str> {
    let header = first_line(patch)?.strip_prefix('[')?.strip_suffix(']')?;
    let path = header.split('#').next().unwrap_or(header);
    (!path.is_empty()).then_some(path)
}

fn tool_result(message: &Value) -> Line {
    let Some(call) = message.get("toolCallId").and_then(Value::as_str) else {
        return Line::Other;
    };
    Line::ToolResults(vec![ToolResult {
        call: call.to_string(),
        failed: message
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        output: message.get("content").and_then(result_tail),
    }])
}

fn kind(name: &str) -> AgentActivityToolKind {
    use AgentActivityToolKind::*;
    // Some omp builds register their tools under an underscore prefix.
    match name.strip_prefix('_').unwrap_or(name) {
        "bash" | "eval" => Shell,
        "read" => Read,
        "edit" | "write" | "ast_edit" => Edit,
        "grep" | "glob" | "find" | "ast_grep" => Search,
        "web_search" | "read_url" | "fetch" | "browser" => Web,
        "task" | "task_batch" => Agent,
        "todo" => Todo,
        "ask" => Question,
        _ => mcp_kind(name),
    }
}
