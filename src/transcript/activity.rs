//! What an agent is doing in its current turn, read from its transcript: the user's prompt,
//! the notes the assistant wrote, its tool calls with their results and, once the turn is
//! over, its final message. A session that opens with a message that is not the user's, such
//! as the context of a handoff, has that message open its first turn instead. The turns before
//! the current one come out of the same lines, a page at a time, in `history`.
//!
//! **Tail strategy.** The current turn sits at the end of a transcript that can be tens of
//! megabytes. One backward pass, in the chunks `ReverseLines` reads, parses each line from
//! the end of the file until it meets the newest real user prompt, so a read costs the size
//! of the current turn, never the size of the file, and nothing is cached between calls.
//! The pass keeps file order instead of following parent links: Claude Code writes the
//! results of parallel tool calls as siblings of one another, so a parent walk would lose
//! all but one of them. `history` has to skip the turns of abandoned branches, so it
//! follows the links, and takes tool results by file order for the same reason.
//!
//! **Cursor.** `c:<turn>.<prompt>.<end>`, each part in hex: the byte offset of the turn's
//! prompt, a hash of the prompt's id, and the offset just past the last complete line read.
//! It names the state a client has seen without the server remembering anything. Every
//! entry records the offset of the newest line that created or changed it, so `since`
//! returns the entries at or past the cursor's end offset. A cursor from another turn,
//! another file or the future is answered with the whole turn and `reset`. So is one that
//! more entries changed after than `limit` allows: sending only the newest of them would
//! leave a gap, so the client gets the turn's newest entries to replace what it holds.

use std::collections::HashMap;
use std::io::{self, Read, Seek};
use std::path::Path;

use serde_json::Value;

use super::{ReverseLines, TranscriptFormat, READ_CHUNK};
use crate::api::schema::{
    AgentActivityEntry, AgentActivityEntryKind, AgentActivityQuestion, AgentActivityQuestionItem,
    AgentActivityQuestionOption, AgentActivityTool, AgentActivityToolKind, AgentActivityToolStatus,
    AgentActivityTurn,
};

mod claude;
mod history;
mod omp;
mod scan;
#[cfg(test)]
mod tests;

pub use history::{history, DEFAULT_TURNS, MAX_TURNS};
use scan::Scan;

/// Entries returned when a request names no limit.
pub const DEFAULT_LIMIT: usize = 200;
/// The most entries one request may ask for.
pub const MAX_LIMIT: usize = 500;

const SUMMARY_CHARS: usize = 120;
const TARGET_CHARS: usize = 200;
const OUTPUT_LINES: usize = 5;
const OUTPUT_CHARS: usize = 600;
/// A result's last lines are cleaned up from at most this many trailing bytes, so one
/// enormous line costs no more than a short one.
const OUTPUT_SCAN_BYTES: usize = 64 * 1024;

/// One read of the current turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub turn: AgentActivityTurn,
    /// The turn's entries in order, or only those added or changed since the cursor given.
    pub entries: Vec<AgentActivityEntry>,
    pub cursor: String,
    /// The cursor given no longer applies, or more entries changed since than the limit
    /// allows: `entries` holds the turn, its newest ones up to the limit, and replaces what
    /// the client holds.
    pub reset: bool,
    /// Older entries of the turn were left out to respect the limit.
    pub truncated: bool,
}

/// Reads the current turn of a transcript: from its newest real user prompt onwards, or from
/// the context the session opened with while no prompt follows it. `Ok(None)` means the
/// transcript holds no turn yet. `limit` is at least 1. With
/// `thinking`, the assistant's thinking is among the entries.
pub fn activity(
    format: TranscriptFormat,
    path: &Path,
    since: Option<&str>,
    limit: usize,
    thinking: bool,
) -> io::Result<Option<Activity>> {
    let lines = ReverseLines::new(std::fs::File::open(path)?, READ_CHUNK)?;
    Ok(read_turn(format, lines, thinking)?.map(|turn| turn.into_activity(since, limit)))
}

/// One transcript line reduced to what the feed needs.
enum Line {
    Prompt(Prompt),
    /// A message the session gave the model that is not the user's, such as the context of a
    /// handoff. The one a session opens with opens its first turn (see `Scan`); later ones are
    /// not part of the feed.
    Context(Prompt),
    /// A message the user sent while the agent was working. It opens no turn.
    MidPrompt(Prompt),
    /// Where the agent summarised the conversation so far.
    Compaction(Compaction),
    /// What a local slash command printed. The model never saw it.
    LocalOutput,
    /// The user stopped the agent.
    Interrupt,
    Assistant(Assistant),
    ToolResults(Vec<ToolResult>),
    /// Everything else: bookkeeping, attachments, thinking-only noise, lines this reader
    /// does not understand.
    Other,
}

/// A parsed transcript line and how it links to the others.
struct Parsed {
    /// `None` when the line is no entry of the main conversation: torn by a concurrent write,
    /// or written by a subagent.
    link: Option<Link>,
    line: Line,
}

impl Parsed {
    fn other() -> Self {
        Self {
            link: None,
            line: Line::Other,
        }
    }
}

/// How an entry links to the rest of its transcript. Transcripts are trees: a session that
/// is rewound or branched keeps the abandoned branch in the file.
struct Link {
    id: String,
    /// The entry this one follows. `None` for the first entry of a branch.
    parent: Option<String>,
    /// The entry may be the newest of its branch. Entries that hang off the side of one and
    /// lead nowhere, such as hook results, are not.
    tip: bool,
    /// A tool result. Claude Code writes the results of parallel calls as siblings of one
    /// another, of which the branch goes on through only one, so results count wherever
    /// they hang.
    result: bool,
}

/// A user message that opens a turn, or that the user sent in the middle of one, or the
/// context a session opened with.
struct Prompt {
    id: String,
    timestamp: Option<String>,
    text: String,
    /// How many images came with it.
    images: u32,
    /// Typed as a slash command. A local one (`/model`, `/clear`) prints its result and never
    /// reaches the model, so it opens a turn only when that output does not follow it.
    command: bool,
    /// Not the user's words: the context a session opened with.
    context: bool,
}

struct Compaction {
    id: String,
    timestamp: Option<String>,
    /// The summary, when the transcript has it.
    text: Option<String>,
}

struct Assistant {
    /// Claude Code writes each content block of one response as its own line, all sharing
    /// the response's message id. omp writes a response as one line and has no such key.
    response: Option<String>,
    timestamp: Option<String>,
    stop: Stop,
    blocks: Vec<Block>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// The response ended in tool calls.
    ToolUse,
    /// The model finished what it was saying.
    Ended,
    /// The transcript records no stop reason: the user cut the response short.
    Unknown,
}

enum Block {
    Text {
        id: String,
        text: String,
    },
    /// The assistant's thinking. Only read when the request asked for it.
    Thinking {
        id: String,
        text: String,
    },
    Call(Call),
}

struct Call {
    id: String,
    /// The call as it reads before its result arrives.
    tool: AgentActivityTool,
}

struct ToolResult {
    call: String,
    failed: bool,
    /// The tail of the result, already cut down.
    output: Option<String>,
    /// How many images the result carried.
    images: u32,
}

fn parse(format: TranscriptFormat, line: &[u8], thinking: bool) -> Parsed {
    match format {
        TranscriptFormat::Claude => claude::parse(line, thinking),
        TranscriptFormat::Omp => omp::parse(line, thinking),
    }
}

/// Parses one transcript line. Writers that escape text the way JavaScript does leave an
/// unpaired surrogate such as `\ud83d` behind when they cut a result in the middle of an
/// emoji, and JSON parsers reject it. The line is still worth reading, with U+FFFD where the
/// surrogate was.
fn parse_entry<T: serde::de::DeserializeOwned>(line: &[u8]) -> Option<T> {
    serde_json::from_slice(line).ok().or_else(|| {
        let repaired = repair_surrogates(line)?;
        serde_json::from_slice(&repaired).ok()
    })
}

/// A copy of `line` with every unpaired UTF-16 surrogate escape replaced by `\ufffd`, which
/// is as long, or `None` when there is nothing to replace.
fn repair_surrogates(line: &[u8]) -> Option<Vec<u8>> {
    // The UTF-16 code unit of the `\uXXXX` escape at `at`, when it is a surrogate.
    let surrogate = |at: usize| -> Option<u16> {
        if line.get(at..at + 2)? != b"\\u" {
            return None;
        }
        let digits = std::str::from_utf8(line.get(at + 2..at + 6)?).ok()?;
        u16::from_str_radix(digits, 16)
            .ok()
            .filter(|unit| (0xd800..=0xdfff).contains(unit))
    };
    let mut repaired: Option<Vec<u8>> = None;
    let mut at = 0;
    while at < line.len() {
        if line[at] != b'\\' {
            at += 1;
            continue;
        }
        // At a backslash at least two bytes are consumed, so the `u` of an escaped
        // backslash (`\\u`) is never taken for the start of a `\u` escape.
        match surrogate(at) {
            Some(0xd800..=0xdbff)
                if surrogate(at + 6).is_some_and(|low| (0xdc00..=0xdfff).contains(&low)) =>
            {
                at += 12;
            }
            Some(_) => {
                repaired.get_or_insert_with(|| line.to_vec())[at..at + 6]
                    .copy_from_slice(b"\\ufffd");
                at += 6;
            }
            None => at += 2,
        }
    }
    repaired
}

/// Reads backwards from the end of the transcript to the newest real user prompt and builds
/// the turn from the lines after it.
fn read_turn<R: Read + Seek>(
    format: TranscriptFormat,
    lines: ReverseLines<R>,
    thinking: bool,
) -> io::Result<Option<Turn>> {
    Scan::in_file_order(format, lines, thinking).next_turn()
}

/// A turn as the transcript stands.
struct Turn {
    /// Offset of the prompt's line.
    start: u64,
    /// Offset just past the last complete line read.
    end: u64,
    prompt: Prompt,
    responses: Vec<Response>,
    /// Index into `responses` by response key.
    by_key: HashMap<String, usize>,
    results: HashMap<String, Outcome>,
    /// Offset of the newest line holding a tool result.
    last_result: u64,
    /// Offset of the line where the user stopped the agent, when no response followed it.
    interrupted_at: Option<u64>,
    /// What sits between the responses, with the offset of its line: messages the user sent
    /// mid-turn, and compactions.
    inserted: Vec<(u64, AgentActivityEntry)>,
}

struct Response {
    last_line: u64,
    stop: Stop,
    items: Vec<Item>,
    /// Offset of the line that began the next response.
    superseded_at: Option<u64>,
}

impl Response {
    fn has_call(&self) -> bool {
        self.items
            .iter()
            .any(|item| matches!(item.block, Block::Call(_)))
    }
}

struct Item {
    line: u64,
    timestamp: Option<String>,
    block: Block,
}

struct Outcome {
    line: u64,
    failed: bool,
    output: Option<String>,
    /// How many images the result carried.
    images: u32,
}

/// An entry of a turn and where it belongs.
struct Placed {
    /// Offset of the line that wrote it, which is its place among the other entries.
    at: u64,
    /// Offset of the newest line that created or changed it.
    touched: u64,
    entry: AgentActivityEntry,
}

impl Turn {
    fn new(start: u64, end: u64, prompt: Prompt) -> Self {
        Self {
            start,
            end,
            prompt,
            responses: Vec::new(),
            by_key: HashMap::new(),
            results: HashMap::new(),
            last_result: 0,
            interrupted_at: None,
            inserted: Vec::new(),
        }
    }

    /// Adds the next line of the turn, in file order.
    fn push(&mut self, offset: u64, line: Line) {
        match line {
            Line::Assistant(assistant) => self.push_assistant(offset, assistant),
            Line::ToolResults(results) => {
                self.last_result = offset;
                for result in results {
                    self.results.insert(
                        result.call,
                        Outcome {
                            line: offset,
                            failed: result.failed,
                            output: result.output,
                            images: result.images,
                        },
                    );
                }
            }
            Line::Interrupt => self.interrupted_at = Some(offset),
            Line::MidPrompt(prompt) => self.inserted.push((offset, prompt_entry(&prompt))),
            Line::Compaction(compaction) => {
                self.inserted.push((offset, compaction_entry(&compaction)))
            }
            Line::Prompt(_) | Line::Context(_) | Line::LocalOutput | Line::Other => {}
        }
    }

    fn push_assistant(&mut self, offset: u64, assistant: Assistant) {
        let known = assistant
            .response
            .as_ref()
            .and_then(|key| self.by_key.get(key))
            .copied();
        let index = match known {
            Some(index) => index,
            None => {
                if let Some(previous) = self.responses.last_mut() {
                    previous.superseded_at.get_or_insert(offset);
                }
                self.interrupted_at = None;
                self.responses.push(Response {
                    last_line: offset,
                    stop: Stop::Unknown,
                    items: Vec::new(),
                    superseded_at: None,
                });
                let index = self.responses.len() - 1;
                if let Some(key) = assistant.response {
                    self.by_key.insert(key, index);
                }
                index
            }
        };
        let response = &mut self.responses[index];
        response.last_line = offset;
        if assistant.stop != Stop::Unknown {
            response.stop = assistant.stop;
        }
        let timestamp = assistant.timestamp;
        response
            .items
            .extend(assistant.blocks.into_iter().map(|block| Item {
                line: offset,
                timestamp: timestamp.clone(),
                block,
            }));
    }

    /// The turn's entries in order, each with where its line sits in the file and the offset
    /// of the newest line that created or changed it, and whether the turn is over.
    ///
    /// A response without tool calls becomes one entry holding all its text: the final
    /// `message` when it closes a finished turn, a `note` otherwise. Its kind can change
    /// with later lines, which is why those lines count as changing it. Thinking stands in
    /// entries of its own. Messages the user sent mid-turn and compactions go where their
    /// lines were written.
    fn entries(&self) -> (Vec<Placed>, bool) {
        let interrupted = self.interrupted_at.is_some();
        let running = self
            .responses
            .iter()
            .flat_map(|response| &response.items)
            .any(|item| {
                matches!(&item.block, Block::Call(call) if !self.results.contains_key(&call.id))
            });
        let last = self.responses.len().checked_sub(1);
        // The response that ends the turn: the model finished it, or the user cut it short.
        let closing = last.filter(|&index| {
            let response = &self.responses[index];
            !response.has_call()
                && response.stop != Stop::ToolUse
                && (response.stop == Stop::Ended || interrupted)
        });
        let finished = (closing.is_some() || interrupted) && !running;

        let mut entries = vec![Placed {
            at: self.start,
            touched: self.start,
            entry: prompt_entry(&self.prompt),
        }];
        for (index, response) in self.responses.iter().enumerate() {
            if response.has_call() {
                // The text of a response that holds tool calls was one entry until its
                // first call was written; from that line on, each block is an entry.
                let first_call = response
                    .items
                    .iter()
                    .filter(|item| matches!(item.block, Block::Call(_)))
                    .map(|item| item.line)
                    .min()
                    .unwrap_or(0);
                for item in &response.items {
                    entries.push(match &item.block {
                        Block::Text { id, text } => Placed {
                            at: item.line,
                            touched: item.line.max(first_call),
                            entry: text_entry(
                                id,
                                &item.timestamp,
                                text.clone(),
                                AgentActivityEntryKind::Note,
                            ),
                        },
                        Block::Thinking { id, text } => thinking_entry(item, id, text),
                        Block::Call(call) => self.tool_entry(item, call),
                    });
                }
                continue;
            }
            let mut texts = response.items.iter().filter_map(|item| match &item.block {
                Block::Text { id, text } => Some((item, id, text)),
                Block::Thinking { .. } | Block::Call(_) => None,
            });
            let merged = texts.next().map(|(first, id, first_text)| {
                let text = texts.fold(first_text.clone(), |mut text, (_, _, more)| {
                    text.push_str("\n\n");
                    text.push_str(more);
                    text
                });
                (first, id, text)
            });
            let kind = if finished && closing == Some(index) {
                AgentActivityEntryKind::Message
            } else {
                AgentActivityEntryKind::Note
            };
            let mut touched = response.last_line.max(response.superseded_at.unwrap_or(0));
            if last == Some(index) {
                touched = touched.max(self.interrupted_at.unwrap_or(0));
            }
            if closing == Some(index) {
                touched = touched.max(self.last_result);
            }
            let mut merged = merged.map(|(first, id, text)| Placed {
                at: first.line,
                touched,
                entry: text_entry(id, &first.timestamp, text, kind),
            });
            // The text entry stands where the first of its blocks was written.
            let first_text = response
                .items
                .iter()
                .position(|item| matches!(item.block, Block::Text { .. }));
            for (position, item) in response.items.iter().enumerate() {
                match &item.block {
                    Block::Thinking { id, text } => entries.push(thinking_entry(item, id, text)),
                    Block::Text { .. } if Some(position) == first_text => {
                        entries.extend(merged.take());
                    }
                    Block::Text { .. } | Block::Call(_) => {}
                }
            }
        }
        for (at, entry) in &self.inserted {
            let index = entries
                .iter()
                .rposition(|placed| placed.at < *at)
                .map_or(0, |index| index + 1);
            entries.insert(
                index,
                Placed {
                    at: *at,
                    touched: *at,
                    entry: entry.clone(),
                },
            );
        }
        (entries, finished)
    }

    fn tool_entry(&self, item: &Item, call: &Call) -> Placed {
        let mut tool = call.tool.clone();
        let mut touched = item.line;
        let mut images = None;
        if let Some(outcome) = self.results.get(&call.id) {
            touched = touched.max(outcome.line);
            tool.status = if outcome.failed {
                AgentActivityToolStatus::Failed
            } else {
                AgentActivityToolStatus::Succeeded
            };
            tool.output = outcome.output.clone();
            images = (outcome.images > 0).then_some(outcome.images);
        }
        Placed {
            at: item.line,
            touched,
            entry: AgentActivityEntry {
                id: call.id.clone(),
                kind: AgentActivityEntryKind::Tool,
                timestamp: item.timestamp.clone(),
                text: None,
                tool: Some(tool),
                images,
            },
        }
    }

    /// Applies the client's cursor and limit to the turn.
    fn into_activity(self, since: Option<&str>, limit: usize) -> Activity {
        let fingerprint = fingerprint(&self.prompt.id);
        let (entries, finished) = self.entries();
        let mut reset = false;
        let mut resume = None;
        if let Some(since) = since {
            match Cursor::decode(since) {
                Some(cursor)
                    if cursor.start == self.start
                        && cursor.fingerprint == fingerprint
                        && cursor.end <= self.end =>
                {
                    resume = Some(cursor.end);
                }
                _ => reset = true,
            }
        }
        // More changes than the limit allows would leave a gap between what the client holds
        // and the newest entries it is sent. It gets the turn's newest entries to replace what
        // it holds instead, as if the cursor no longer applied.
        if let Some(end) = resume {
            let changed = entries
                .iter()
                .filter(|placed| placed.touched >= end)
                .count();
            if changed > limit {
                resume = None;
                reset = true;
            }
        }
        let mut entries: Vec<AgentActivityEntry> = entries
            .into_iter()
            .filter(|placed| resume.is_none_or(|end| placed.touched >= end))
            .map(|placed| placed.entry)
            .collect();
        let truncated = entries.len() > limit;
        if truncated {
            entries.drain(..entries.len() - limit);
        }
        Activity {
            turn: AgentActivityTurn {
                id: Some(self.prompt.id),
                started_at: self.prompt.timestamp,
                finished,
            },
            entries,
            cursor: Cursor {
                start: self.start,
                fingerprint,
                end: self.end,
            }
            .encode(),
            reset,
            truncated,
        }
    }
}

fn text_entry(
    id: &str,
    timestamp: &Option<String>,
    text: String,
    kind: AgentActivityEntryKind,
) -> AgentActivityEntry {
    AgentActivityEntry {
        id: id.to_string(),
        kind,
        timestamp: timestamp.clone(),
        text: Some(text),
        tool: None,
        images: None,
    }
}

fn thinking_entry(item: &Item, id: &str, text: &str) -> Placed {
    Placed {
        at: item.line,
        touched: item.line,
        entry: text_entry(
            id,
            &item.timestamp,
            text.to_string(),
            AgentActivityEntryKind::Thinking,
        ),
    }
}

fn prompt_entry(prompt: &Prompt) -> AgentActivityEntry {
    AgentActivityEntry {
        id: prompt.id.clone(),
        kind: if prompt.context {
            AgentActivityEntryKind::Context
        } else {
            AgentActivityEntryKind::Prompt
        },
        timestamp: prompt.timestamp.clone(),
        text: Some(prompt.text.clone()),
        tool: None,
        images: (prompt.images > 0).then_some(prompt.images),
    }
}

fn compaction_entry(compaction: &Compaction) -> AgentActivityEntry {
    AgentActivityEntry {
        id: compaction.id.clone(),
        kind: AgentActivityEntryKind::Compaction,
        timestamp: compaction.timestamp.clone(),
        text: compaction.text.clone(),
        tool: None,
        images: None,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Cursor {
    start: u64,
    fingerprint: u64,
    end: u64,
}

impl Cursor {
    fn encode(&self) -> String {
        format!("c:{:x}.{:x}.{:x}", self.start, self.fingerprint, self.end)
    }

    fn decode(text: &str) -> Option<Self> {
        let mut parts = text.strip_prefix("c:")?.split('.');
        let mut part = || u64::from_str_radix(parts.next()?, 16).ok();
        let cursor = Self {
            start: part()?,
            fingerprint: part()?,
            end: part()?,
        };
        parts.next().is_none().then_some(cursor)
    }
}

/// FNV-1a. Cursors must mean the same after a server restart, which rules out the standard
/// library's hasher.
fn fingerprint(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Which end of a text survives when it is too long.
#[derive(Clone, Copy)]
enum Keep {
    Head,
    Tail,
}

/// Cuts `text` to at most `max` characters, marking the cut with an ellipsis.
fn shorten(text: &str, max: usize, keep: Keep) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    match keep {
        Keep::Head => {
            let head: String = text.chars().take(max - 1).collect();
            format!("{head}…")
        }
        Keep::Tail => {
            let tail: String = text.chars().skip(count - (max - 1)).collect();
            format!("…{tail}")
        }
    }
}

/// The first line with anything on it, trimmed.
fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// The words of a user message: its text blocks, or its plain text, without the reminders an
/// agent slips in beside them. `None` when nothing is left.
fn message_text(content: Option<&Value>) -> Option<String> {
    let text = match content? {
        Value::String(text) => text.trim().to_string(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text")?.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty() && !text.starts_with("<system-reminder>"))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

/// How many image blocks a message's content holds.
fn count_images(content: Option<&Value>) -> u32 {
    let Some(Value::Array(blocks)) = content else {
        return 0;
    };
    let images = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .count();
    u32::try_from(images).unwrap_or(u32::MAX)
}

/// How a tool call reads in the feed, before its result arrives. `intent` is what the agent
/// said it was doing, for transcripts that record that.
fn describe(
    name: &str,
    kind: AgentActivityToolKind,
    input: Option<&Value>,
    intent: Option<&str>,
) -> AgentActivityTool {
    let text = |key: &str| {
        input
            .and_then(|input| input.get(key))
            .and_then(Value::as_str)
            .and_then(first_line)
    };
    let command = text("command");
    let path = text("file_path")
        .or_else(|| text("notebook_path"))
        .or_else(|| text("path"));
    let pattern = text("pattern");
    let url = text("url");
    let searching = kind == AgentActivityToolKind::Search;

    let (target, target_keep) = match kind {
        AgentActivityToolKind::Shell => (command, Keep::Head),
        AgentActivityToolKind::Search => (path.or(pattern), Keep::Tail),
        AgentActivityToolKind::Web => (url.or_else(|| text("query")), Keep::Tail),
        _ => (path.or(url), Keep::Tail),
    };
    // A search is about its pattern before its place. Past the fields every agent uses,
    // `query`, `skill` and `title` say more than the bare tool name.
    let candidates = [
        (intent.and_then(first_line), Keep::Head),
        (text("description"), Keep::Head),
        (command, Keep::Head),
        (searching.then_some(pattern).flatten(), Keep::Head),
        (path, Keep::Tail),
        (pattern, Keep::Head),
        (url, Keep::Tail),
        (text("query"), Keep::Head),
        (text("skill"), Keep::Head),
        (text("title"), Keep::Head),
    ];
    let summary = candidates
        .into_iter()
        .find_map(|(text, keep)| text.map(|text| shorten(text, SUMMARY_CHARS, keep)))
        .unwrap_or_else(|| {
            shorten(
                if name.is_empty() { "tool" } else { name },
                SUMMARY_CHARS,
                Keep::Head,
            )
        });

    AgentActivityTool {
        name: name.to_string(),
        kind,
        summary,
        target: target.map(|text| shorten(text, TARGET_CHARS, target_keep)),
        status: AgentActivityToolStatus::Running,
        output: None,
        question: if kind == AgentActivityToolKind::Question {
            questions(input)
        } else {
            None
        },
    }
}

/// The questions an ask-the-user call puts to the user, from its input.
fn questions(input: Option<&Value>) -> Option<AgentActivityQuestion> {
    let questions: Vec<AgentActivityQuestionItem> = input?
        .get("questions")?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let question = item.get("question")?.as_str()?.trim();
            if question.is_empty() {
                return None;
            }
            Some(AgentActivityQuestionItem {
                question: question.to_string(),
                header: item
                    .get("header")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|header| !header.is_empty())
                    .map(str::to_string),
                multi_select: item
                    .get("multiSelect")
                    .or_else(|| item.get("multi"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                options: item
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|options| options.iter().filter_map(question_option).collect())
                    .unwrap_or_default(),
            })
        })
        .collect();
    (!questions.is_empty()).then_some(AgentActivityQuestion { questions })
}

fn question_option(option: &Value) -> Option<AgentActivityQuestionOption> {
    let (label, description) = match option {
        Value::String(label) => (label.as_str(), None),
        Value::Object(_) => (
            option.get("label")?.as_str()?,
            option.get("description").and_then(Value::as_str),
        ),
        _ => return None,
    };
    let label = label.trim();
    (!label.is_empty()).then(|| AgentActivityQuestionOption {
        label: label.to_string(),
        description: description
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map(str::to_string),
    })
}

/// MCP tools are named `mcp__<server>__<tool>`. The server says what kind of tool it is:
/// browser servers are the web.
fn mcp_kind(name: &str) -> AgentActivityToolKind {
    let server = name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split("__").next())
        .unwrap_or_default();
    if ["playwright", "puppeteer", "chrome", "browser"]
        .iter()
        .any(|browser| server.contains(browser))
    {
        AgentActivityToolKind::Web
    } else {
        AgentActivityToolKind::Other
    }
}

/// The end of a tool result's text: its last `OUTPUT_LINES` lines and `OUTPUT_CHARS`
/// characters, without terminal control sequences. `None` when nothing is left.
fn result_tail(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => output_tail(text),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text")?.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            output_tail(&text)
        }
        _ => None,
    }
}

fn output_tail(text: &str) -> Option<String> {
    let text = text.trim_end();
    // The last lines, found without splitting what may be megabytes of output.
    let start = text
        .rmatch_indices('\n')
        .nth(OUTPUT_LINES - 1)
        .map_or(0, |(index, _)| index + 1);
    let mut tail = &text[start..];
    if tail.len() > OUTPUT_SCAN_BYTES {
        let mut cut = tail.len() - OUTPUT_SCAN_BYTES;
        while !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail = &tail[cut..];
    }
    let stripped = crate::ansi::strip_terminal_control_sequences(tail.as_bytes());
    let cleaned = String::from_utf8_lossy(&stripped)
        .split('\n')
        .map(visible_line)
        .collect::<Vec<_>>()
        .join("\n");
    let cleaned = cleaned.trim_end();
    if cleaned.is_empty() {
        return None;
    }
    let count = cleaned.chars().count();
    if count <= OUTPUT_CHARS {
        return Some(cleaned.to_string());
    }
    // Keep the last characters and mark the cut, within the character budget.
    let skip = count - (OUTPUT_CHARS - 1);
    let index = cleaned
        .char_indices()
        .nth(skip)
        .map_or(cleaned.len(), |(index, _)| index);
    Some(format!("…{}", &cleaned[index..]))
}

/// A line as a terminal would finally show it: a carriage return rewrites the line, as
/// progress bars do, and other control characters print nothing.
fn visible_line(line: &str) -> String {
    let line = line.trim_end_matches('\r');
    line.rsplit('\r')
        .next()
        .unwrap_or(line)
        .chars()
        .filter(|ch| !ch.is_control() || *ch == '\t')
        .collect()
}
