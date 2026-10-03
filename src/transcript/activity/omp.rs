//! omp transcripts: one JSON object per line, forming a tree by `id` and `parentId`. A
//! `message` line holds a whole response or a whole tool result. A prompt the user sent
//! through a skill or a collaborator is a `custom_message` line instead of a user message.
//! What the user typed while the agent was working is a user message marked `steering`.

use serde::Deserialize;
use serde_json::Value;

use super::{
    count_images, describe, first_line, mcp_kind, message_text, parse_entry, result_tail, shorten,
    Assistant, Block, Call, Compaction, Keep, Line, Link, Parsed, Prompt, Stop, ToolResult,
    TARGET_CHARS,
};
use crate::api::schema::AgentActivityToolKind;

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "parentId", default)]
    parent_id: Option<String>,
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
    /// The text of a `compaction` line.
    #[serde(default)]
    summary: Option<String>,
}

pub(super) fn parse(line: &[u8], thinking: bool) -> Parsed {
    let Some(entry) = parse_entry::<Entry>(line) else {
        // A line torn by a concurrent write is not an entry.
        return Parsed::other();
    };
    let line = line_of(&entry, thinking);
    // Every entry belongs to the tree, whatever it records.
    let link = entry.id.map(|id| Link {
        id,
        parent: entry.parent_id,
        tip: true,
        result: false,
    });
    Parsed { link, line }
}

fn line_of(entry: &Entry, thinking: bool) -> Line {
    match entry.kind.as_deref() {
        Some("message") => {}
        Some("custom_message") => return custom_prompt(entry),
        Some("compaction") => return compaction(entry),
        _ => return Line::Other,
    }
    let (Some(id), Some(message)) = (entry.id.as_deref(), &entry.message) else {
        return Line::Other;
    };
    match message.get("role").and_then(Value::as_str) {
        Some("user") => user(id, entry.timestamp.clone(), message),
        Some("assistant") => assistant(id, entry.timestamp.clone(), message, thinking),
        Some("developer") => context(id, entry.timestamp.clone(), message),
        Some("toolResult") => tool_result(message),
        _ => Line::Other,
    }
}

fn user(id: &str, timestamp: Option<String>, message: &Value) -> Line {
    let content = message.get("content");
    let Some(text) = message_text(content) else {
        return Line::Other;
    };
    let prompt = Prompt {
        id: id.to_string(),
        timestamp,
        text,
        images: count_images(content),
        command: false,
        context: false,
    };
    // What the user typed while the agent was working is marked as steering. It reaches the
    // model after the tool calls that were running, and continues the turn.
    if message
        .get("steering")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        Line::MidPrompt(prompt)
    } else {
        Line::Prompt(prompt)
    }
}

/// A message omp gives the model on the session's behalf, such as the context of a handoff.
/// A session that opens with one has it open its first turn.
fn context(id: &str, timestamp: Option<String>, message: &Value) -> Line {
    let content = message.get("content");
    match message_text(content) {
        Some(text) => Line::Context(Prompt {
            id: id.to_string(),
            timestamp,
            text,
            images: count_images(content),
            command: false,
            context: true,
        }),
        None => Line::Other,
    }
}

/// Where omp summarised the conversation so far.
fn compaction(entry: &Entry) -> Line {
    let Some(id) = entry.id.clone() else {
        return Line::Other;
    };
    Line::Compaction(Compaction {
        id,
        timestamp: entry.timestamp.clone(),
        text: entry
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|summary| !summary.is_empty())
            .map(str::to_string),
    })
}

/// A prompt that reaches omp as a custom message: a skill the user invoked, a message from a
/// collaborator. What the user sees is what counts. Notices the agent sends itself, and
/// attachments that have no display, are not prompts.
fn custom_prompt(entry: &Entry) -> Line {
    if entry.attribution.as_deref() != Some("user") || entry.display != Some(true) {
        return Line::Other;
    }
    let Some(id) = entry.id.clone() else {
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
            timestamp: entry.timestamp.clone(),
            text,
            images: count_images(entry.content.as_ref()),
            command: false,
            context: false,
        }),
        None => Line::Other,
    }
}

fn assistant(id: &str, timestamp: Option<String>, message: &Value, thinking: bool) -> Line {
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
                .filter_map(|(index, block)| block_of(id, index, block, thinking))
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

/// Every block kind this reader does not know is not part of the feed, and thinking is only
/// when it was asked for.
fn block_of(id: &str, index: usize, block: &Value, thinking: bool) -> Option<Block> {
    match block.get("type")?.as_str()? {
        "text" => {
            let text = block.get("text")?.as_str()?;
            (!text.trim().is_empty()).then(|| Block::Text {
                id: format!("{id}:{index}"),
                text: text.to_string(),
            })
        }
        "thinking" if thinking => {
            let text = block.get("thinking")?.as_str()?;
            (!text.trim().is_empty()).then(|| Block::Thinking {
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
        images: count_images(message.get("content")),
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
