//! Reads an agent's transcript, the file its hook integration reports: the last complete
//! message here, and what the agent is doing in its current turn in `activity`.
//!
//! Transcripts are append-only JSONL files that can grow to tens of megabytes, and the
//! final message is almost always near the end. The readers walk the file backwards and
//! follow the parent links from the newest entry, because sessions branch: entries on an
//! abandoned branch must not count. Parents are always written before their children, so
//! one backwards pass visits the whole active branch.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use serde::Deserialize;

mod activity;

pub use activity::{activity, DEFAULT_LIMIT, MAX_LIMIT};

const READ_CHUNK: usize = 64 * 1024;

/// A transcript format Herdr can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptFormat {
    Omp,
    Claude,
}

impl TranscriptFormat {
    /// The format an agent writes, when Herdr has a reader for it.
    pub fn for_agent(agent: &str) -> Option<Self> {
        match agent {
            "omp" => Some(Self::Omp),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }
}

/// The last complete message of an agent: the final assistant message of its most recent
/// finished turn, never truncated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastMessage {
    /// The message's text blocks, joined by blank lines. Agents write markdown.
    pub text: String,
    /// Why the model stopped, as the transcript records it.
    pub stop_reason: Option<String>,
    /// When the message was written, as the transcript records it.
    pub timestamp: Option<String>,
}

/// Reads the last complete message from a transcript. `Ok(None)` means the transcript has
/// no finished assistant message on its active branch yet.
pub fn last_message(format: TranscriptFormat, path: &Path) -> io::Result<Option<LastMessage>> {
    let lines = ReverseLines::new(std::fs::File::open(path)?, READ_CHUNK)?;
    match format {
        TranscriptFormat::Omp => omp_last_message(lines),
        TranscriptFormat::Claude => claude_last_message(lines),
    }
}

/// Yields the lines of a file from last to first, reading it backwards in growing chunks.
struct ReverseLines<R> {
    reader: R,
    /// File offset of `buf[0]`.
    pos: u64,
    /// The file's length when reading began. Nothing past it is read.
    len: u64,
    buf: Vec<u8>,
    /// `buf[..end]` holds the bytes not yet yielded.
    end: usize,
    chunk: usize,
}

impl<R: Read + Seek> ReverseLines<R> {
    fn new(mut reader: R, chunk: usize) -> io::Result<Self> {
        let pos = reader.seek(SeekFrom::End(0))?;
        Ok(Self {
            reader,
            pos,
            len: pos,
            buf: Vec::new(),
            end: 0,
            chunk,
        })
    }

    /// Prepends the preceding part of the file. Chunks at least double the unyielded bytes,
    /// so one very long line costs linear time.
    fn read_back(&mut self) -> io::Result<()> {
        let size = self.chunk.max(self.end) as u64;
        let size = size.min(self.pos);
        self.pos -= size;
        self.reader.seek(SeekFrom::Start(self.pos))?;
        let mut data = vec![0; size as usize + self.end];
        self.reader.read_exact(&mut data[..size as usize])?;
        data[size as usize..].copy_from_slice(&self.buf[..self.end]);
        self.end = data.len();
        self.buf = data;
        Ok(())
    }

    /// Yields the next line, last to first, with the file offset it starts at.
    fn next_with_offset(&mut self) -> Option<io::Result<(u64, Vec<u8>)>> {
        loop {
            if let Some(newline) = self.buf[..self.end].iter().rposition(|&b| b == b'\n') {
                let start = newline + 1;
                let line = self.buf[start..self.end].to_vec();
                let offset = self.pos + start as u64;
                self.end = newline;
                if line.is_empty() {
                    continue;
                }
                return Some(Ok((offset, line)));
            }
            if self.pos == 0 {
                if self.end == 0 {
                    return None;
                }
                let line = self.buf[..self.end].to_vec();
                self.end = 0;
                return Some(Ok((0, line)));
            }
            if let Err(error) = self.read_back() {
                return Some(Err(error));
            }
        }
    }
}

impl<R: Read + Seek> Iterator for ReverseLines<R> {
    type Item = io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_with_offset()
            .map(|line| line.map(|(_, line)| line))
    }
}

/// Message content: a list of typed blocks, or plain text.
#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Blocks(Vec<ContentBlock>),
    Text(String),
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

impl Content {
    fn texts(self) -> Vec<String> {
        match self {
            Self::Blocks(blocks) => blocks
                .into_iter()
                .filter(|block| block.kind == "text")
                .filter_map(|block| block.text)
                .collect(),
            Self::Text(text) => vec![text],
        }
    }

    fn has_tool_use(&self) -> bool {
        matches!(self, Self::Blocks(blocks) if blocks.iter().any(|block| block.kind == "tool_use"))
    }
}

#[derive(Deserialize)]
struct OmpEntryHead {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "parentId", default)]
    parent_id: Option<String>,
}

#[derive(Deserialize)]
struct OmpMessageEntry {
    #[serde(default)]
    timestamp: Option<String>,
    message: OmpMessage,
}

#[derive(Deserialize)]
struct OmpMessage {
    role: String,
    #[serde(rename = "stopReason", default)]
    stop_reason: Option<String>,
    content: Content,
}

/// omp: the newest assistant message on the active branch whose turn did not stop for a
/// tool call.
fn omp_last_message(
    lines: impl Iterator<Item = io::Result<Vec<u8>>>,
) -> io::Result<Option<LastMessage>> {
    let mut wanted: Option<String> = None;
    for line in lines {
        let line = line?;
        // A torn line from a concurrent write is not an entry.
        let Ok(head) = serde_json::from_slice::<OmpEntryHead>(&line) else {
            continue;
        };
        let Some(id) = head.id else {
            continue;
        };
        if wanted.as_ref().is_some_and(|wanted| *wanted != id) {
            continue;
        }
        if head.kind.as_deref() == Some("message") {
            // Parse details separately so a message this reader does not understand cannot
            // break the parent chain.
            if let Ok(entry) = serde_json::from_slice::<OmpMessageEntry>(&line) {
                if entry.message.role == "assistant"
                    && entry.message.stop_reason.as_deref() != Some("toolUse")
                {
                    return Ok(Some(LastMessage {
                        text: entry.message.content.texts().join("\n\n"),
                        stop_reason: entry.message.stop_reason,
                        timestamp: entry.timestamp,
                    }));
                }
            }
        }
        let Some(parent_id) = head.parent_id else {
            return Ok(None);
        };
        wanted = Some(parent_id);
    }
    Ok(None)
}

#[derive(Deserialize)]
struct ClaudeEntryHead {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(rename = "parentUuid", default)]
    parent_uuid: Option<String>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeAssistantEntry {
    message: ClaudeMessage,
}

#[derive(Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
    content: Content,
}

/// One model response. Claude Code writes each content block of a response as its own
/// entry, all sharing the response's message id.
struct ClaudeResponse {
    id: Option<String>,
    stop_reason: Option<String>,
    timestamp: Option<String>,
    /// Text blocks, newest first.
    texts: Vec<String>,
    has_tool_use: bool,
}

impl ClaudeResponse {
    fn is_final(&self) -> bool {
        !self.has_tool_use && self.stop_reason.as_deref() != Some("tool_use")
    }

    fn into_last_message(mut self) -> LastMessage {
        self.texts.reverse();
        LastMessage {
            text: self.texts.join("\n\n"),
            stop_reason: self.stop_reason,
            timestamp: self.timestamp,
        }
    }
}

/// Claude Code: the newest response on the main chain that did not stop for a tool call.
/// Subagent entries are sidechains and never count.
fn claude_last_message(
    lines: impl Iterator<Item = io::Result<Vec<u8>>>,
) -> io::Result<Option<LastMessage>> {
    let mut wanted: Option<String> = None;
    let mut response: Option<ClaudeResponse> = None;
    for line in lines {
        let line = line?;
        let Ok(head) = serde_json::from_slice::<ClaudeEntryHead>(&line) else {
            continue;
        };
        let Some(uuid) = head.uuid else {
            continue;
        };
        match &wanted {
            Some(wanted) if *wanted != uuid => continue,
            None if head.is_sidechain => continue,
            _ => {}
        }
        let assistant = (head.kind.as_deref() == Some("assistant"))
            .then(|| serde_json::from_slice::<ClaudeAssistantEntry>(&line).ok())
            .flatten()
            .map(|entry| entry.message);

        match (response.as_mut(), assistant) {
            (Some(current), Some(message)) if current.id.is_some() && current.id == message.id => {
                current.has_tool_use |= message.content.has_tool_use();
                current
                    .texts
                    .extend(message.content.texts().into_iter().rev());
            }
            (_, assistant) => {
                if let Some(finished) = response.take() {
                    if finished.is_final() {
                        return Ok(Some(finished.into_last_message()));
                    }
                }
                response = assistant
                    .filter(|message| message.stop_reason.as_deref() != Some("tool_use"))
                    .map(|message| ClaudeResponse {
                        has_tool_use: message.content.has_tool_use(),
                        id: message.id,
                        stop_reason: message.stop_reason,
                        timestamp: head.timestamp,
                        texts: message.content.texts().into_iter().rev().collect(),
                    });
            }
        }

        let Some(parent_uuid) = head.parent_uuid else {
            break;
        };
        wanted = Some(parent_uuid);
    }
    Ok(response
        .filter(ClaudeResponse::is_final)
        .map(ClaudeResponse::into_last_message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(text: &str, chunk: usize) -> ReverseLines<io::Cursor<Vec<u8>>> {
        ReverseLines::new(io::Cursor::new(text.as_bytes().to_vec()), chunk).unwrap()
    }

    fn omp(text: &str) -> Option<LastMessage> {
        omp_last_message(lines_of(text, READ_CHUNK)).unwrap()
    }

    fn claude(text: &str) -> Option<LastMessage> {
        claude_last_message(lines_of(text, READ_CHUNK)).unwrap()
    }

    fn omp_entry(id: &str, parent: Option<&str>, message: serde_json::Value) -> String {
        serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": parent,
            "timestamp": format!("t-{id}"),
            "message": message,
        })
        .to_string()
    }

    fn omp_assistant(id: &str, parent: &str, stop: &str, texts: &[&str]) -> String {
        let mut content = vec![serde_json::json!({"type": "thinking", "thinking": "hmm"})];
        content.extend(
            texts
                .iter()
                .map(|text| serde_json::json!({"type": "text", "text": text})),
        );
        if stop == "toolUse" {
            content.push(serde_json::json!({"type": "toolCall", "name": "read"}));
        }
        omp_entry(
            id,
            Some(parent),
            serde_json::json!({"role": "assistant", "stopReason": stop, "content": content}),
        )
    }

    fn omp_user(id: &str, parent: Option<&str>) -> String {
        omp_entry(
            id,
            parent,
            serde_json::json!({"role": "user", "content": "do it"}),
        )
    }

    #[test]
    fn reverse_lines_yield_every_line_last_first_across_chunk_boundaries() {
        let long = "x".repeat(50);
        let text = format!("first\n\n{long}\nthird\nlast-without-newline");
        let lines: Vec<String> = lines_of(&text, 4)
            .map(|line| String::from_utf8(line.unwrap()).unwrap())
            .collect();
        assert_eq!(lines, ["last-without-newline", "third", &long, "first"]);
    }

    #[test]
    fn reverse_lines_report_where_each_line_starts_and_how_long_the_file_is() {
        let long = "x".repeat(50);
        let text = format!("first\n\n{long}\nthird\nlast-without-newline");
        for chunk in [1, 4, 16, READ_CHUNK] {
            let mut lines = lines_of(&text, chunk);
            assert_eq!(lines.len, text.len() as u64);
            let starts: Vec<(u64, String)> = std::iter::from_fn(|| lines.next_with_offset())
                .map(|line| {
                    let (offset, line) = line.unwrap();
                    (offset, String::from_utf8(line).unwrap())
                })
                .collect();
            assert_eq!(starts.len(), 4, "chunk {chunk}");
            for (offset, line) in starts {
                let at = offset as usize;
                assert!(
                    at == 0 || text.as_bytes()[at - 1] == b'\n',
                    "chunk {chunk}: {line:?} does not start a line at {offset}"
                );
                assert!(
                    text[at..].starts_with(&line),
                    "chunk {chunk}: {line:?} is not at {offset}"
                );
            }
        }
    }

    #[test]
    fn omp_returns_the_text_of_the_final_answer_after_tool_calls() {
        let transcript = [
            omp_user("u1", None),
            omp_assistant("a1", "u1", "toolUse", &["Let me look."]),
            omp_entry(
                "r1",
                Some("a1"),
                serde_json::json!({"role": "toolResult", "content": [{"type": "text", "text": "file"}]}),
            ),
            omp_assistant("a2", "r1", "stop", &["## Done", "All **green**."]),
            serde_json::json!({"type": "custom", "id": "c1", "parentId": "a2"}).to_string(),
        ]
        .join("\n");

        let message = omp(&transcript).unwrap();

        assert_eq!(message.text, "## Done\n\nAll **green**.");
        assert_eq!(message.stop_reason.as_deref(), Some("stop"));
        assert_eq!(message.timestamp.as_deref(), Some("t-a2"));
    }

    #[test]
    fn omp_ignores_messages_on_an_abandoned_branch() {
        let transcript = [
            omp_user("u1", None),
            omp_assistant("a1", "u1", "stop", &["kept answer"]),
            omp_user("u2", Some("a1")),
            omp_assistant("a2", "u2", "stop", &["abandoned answer"]),
            // The user went back to a1 and asked something else, still running a tool.
            omp_user("u3", Some("a1")),
            omp_assistant("a3", "u3", "toolUse", &[]),
        ]
        .join("\n");

        assert_eq!(omp(&transcript).unwrap().text, "kept answer");
    }

    #[test]
    fn omp_reports_no_message_before_the_first_finished_turn() {
        let transcript = [
            omp_user("u1", None),
            omp_assistant("a1", "u1", "toolUse", &["Looking."]),
        ]
        .join("\n");

        assert_eq!(omp(&transcript), None);
    }

    fn claude_entry(uuid: &str, parent: Option<&str>, value: serde_json::Value) -> String {
        let mut entry = serde_json::json!({
            "uuid": uuid,
            "parentUuid": parent,
            "isSidechain": false,
            "timestamp": format!("t-{uuid}"),
        });
        entry
            .as_object_mut()
            .unwrap()
            .extend(value.as_object().unwrap().clone());
        entry.to_string()
    }

    fn claude_block(
        uuid: &str,
        parent: &str,
        message_id: &str,
        stop: &str,
        block: serde_json::Value,
    ) -> String {
        claude_entry(
            uuid,
            Some(parent),
            serde_json::json!({
                "type": "assistant",
                "message": {"id": message_id, "role": "assistant", "stop_reason": stop, "content": [block]},
            }),
        )
    }

    fn claude_user(uuid: &str, parent: Option<&str>) -> String {
        claude_entry(
            uuid,
            parent,
            serde_json::json!({"type": "user", "message": {"role": "user", "content": "go"}}),
        )
    }

    #[test]
    fn claude_joins_the_text_blocks_of_the_final_response() {
        let transcript = [
            claude_user("u1", None),
            claude_block(
                "a1",
                "u1",
                "m1",
                "tool_use",
                serde_json::json!({"type": "tool_use", "name": "Bash"}),
            ),
            claude_user("r1", Some("a1")),
            claude_block(
                "a2",
                "r1",
                "m2",
                "end_turn",
                serde_json::json!({"type": "thinking", "thinking": "ok"}),
            ),
            claude_block(
                "a3",
                "a2",
                "m2",
                "end_turn",
                serde_json::json!({"type": "text", "text": "First part."}),
            ),
            claude_block(
                "a4",
                "a3",
                "m2",
                "end_turn",
                serde_json::json!({"type": "text", "text": "Second part."}),
            ),
            serde_json::json!({"type": "last-prompt", "lastPrompt": "go"}).to_string(),
        ]
        .join("\n");

        let message = claude(&transcript).unwrap();

        assert_eq!(message.text, "First part.\n\nSecond part.");
        assert_eq!(message.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(message.timestamp.as_deref(), Some("t-a4"));
    }

    #[test]
    fn claude_skips_a_response_that_ended_in_a_tool_call() {
        let transcript = [
            claude_user("u1", None),
            claude_block(
                "a1",
                "u1",
                "m1",
                "end_turn",
                serde_json::json!({"type": "text", "text": "Earlier answer."}),
            ),
            claude_user("u2", Some("a1")),
            claude_block(
                "a2",
                "u2",
                "m2",
                "tool_use",
                serde_json::json!({"type": "text", "text": "Checking."}),
            ),
            claude_block(
                "a3",
                "a2",
                "m2",
                "tool_use",
                serde_json::json!({"type": "tool_use", "name": "Bash"}),
            ),
        ]
        .join("\n");

        assert_eq!(claude(&transcript).unwrap().text, "Earlier answer.");
    }

    #[test]
    fn claude_ignores_subagent_sidechains() {
        let mut sidechain: serde_json::Value = serde_json::from_str(&claude_block(
            "s1",
            "a1",
            "m9",
            "end_turn",
            serde_json::json!({"type": "text", "text": "Subagent report."}),
        ))
        .unwrap();
        sidechain["isSidechain"] = serde_json::Value::Bool(true);
        let transcript = [
            claude_user("u1", None),
            claude_block(
                "a1",
                "u1",
                "m1",
                "end_turn",
                serde_json::json!({"type": "text", "text": "Main answer."}),
            ),
            sidechain.to_string(),
        ]
        .join("\n");

        assert_eq!(claude(&transcript).unwrap().text, "Main answer.");
    }
}
