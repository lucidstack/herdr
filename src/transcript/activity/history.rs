//! Pages through the turns of a transcript, newest page first, for `agent.history`.
//!
//! **Page.** The newest turns, the current one included, or the turns just older than the one
//! a cursor names. One backward pass from the end of the file, or from the cursor's offset,
//! reads them, so a page costs the size of its turns and a look at what lies before them, and
//! nothing is cached between calls. Only the active branch counts (see `Scan`), so the turns
//! of an abandoned branch never show.
//!
//! **Cursor.** `h:<turn>.<prompt>`, each part in hex: the byte offset where the oldest turn
//! of the page has its prompt, and a hash of that prompt's id. It names the turn, and the next
//! page is the turns before it. It applies only while the line at its offset is that prompt,
//! which another transcript (after `/clear`) or a rewritten file does not satisfy; the
//! answer is then the newest page, with `reset`. A turn's id is its prompt's, the same string
//! `agent.activity` gives the current turn.
//!
//! **Size.** A page holds at most `PAGE_BYTES` of turns as the response writes them, though
//! at least one turn, and a turn at most `MAX_TURN_ENTRIES` entries: its prompt and its
//! newest.

use std::io::{self, Read, Seek, Write};
use std::path::Path;

use super::scan::Scan;
use super::{fingerprint, parse, Line, Link, Turn};
use crate::api::schema::AgentHistoryTurn;
use crate::transcript::{ReverseLines, TranscriptFormat, READ_CHUNK};

/// Turns returned when a request names no count.
pub const DEFAULT_TURNS: usize = 5;
/// The most turns one request may ask for.
pub const MAX_TURNS: usize = 20;

/// The most entries a turn on a page keeps.
const MAX_TURN_ENTRIES: usize = 200;
/// What a page may weigh, as the response writes it, before it is cut short.
const PAGE_BYTES: usize = 512 * 1024;
/// How far back past a page to look for an older turn before taking one to be there.
const LOOK_BACK_BYTES: u64 = 1 << 20;

/// One page of turns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    /// Oldest first.
    pub turns: Vec<AgentHistoryTurn>,
    /// The cursor for the next older page. `None` when this page reaches the start.
    pub before: Option<String>,
    /// The cursor given does not apply, and `turns` is the newest page.
    pub reset: bool,
}

/// Reads a page of turns: the newest `turns`, or those just older than the turn `before`
/// names. `Ok(None)` means the transcript holds no prompt, and `before` was none or did not
/// apply. `turns` is at least 1.
pub fn history(
    format: TranscriptFormat,
    path: &Path,
    before: Option<&str>,
    turns: usize,
    thinking: bool,
) -> io::Result<Option<History>> {
    let lines = ReverseLines::new(std::fs::File::open(path)?, READ_CHUNK)?;
    read_page(format, lines, before, turns, thinking)
}

pub(super) fn read_page<R: Read + Seek>(
    format: TranscriptFormat,
    mut lines: ReverseLines<R>,
    before: Option<&str>,
    max_turns: usize,
    thinking: bool,
) -> io::Result<Option<History>> {
    let resumed = match before.and_then(Before::decode) {
        Some(cursor) => cursor
            .check(format, &mut lines)?
            .map(|link| (cursor.start, link.parent)),
        None => None,
    };
    let continued = resumed.is_some();
    let reset = before.is_some() && !continued;
    let mut scan = match resumed {
        Some((offset, parent)) => Scan::resume(format, lines, thinking, offset, parent),
        None => Scan::on_branch(format, lines, thinking),
    };

    // Newest first, as they are read.
    let mut page = Vec::new();
    let mut bytes = 0;
    let mut oldest = None;
    let mut more = None;
    while page.len() < max_turns {
        let Some(turn) = scan.next_turn()? else {
            more = Some(false);
            break;
        };
        let cursor = Before::of(&turn);
        let turn = turn.into_history();
        let size = json_size(&turn);
        if !page.is_empty() && bytes + size > PAGE_BYTES {
            // This turn is for the next page, which starts with it.
            more = Some(true);
            break;
        }
        bytes += size;
        page.push(turn);
        oldest = Some(cursor);
    }
    let more = match more {
        Some(more) => more,
        None => scan.has_older(LOOK_BACK_BYTES)?,
    };
    if page.is_empty() && !continued {
        return Ok(None);
    }
    page.reverse();
    Ok(Some(History {
        turns: page,
        before: oldest.filter(|_| more).map(|cursor| cursor.encode()),
        reset,
    }))
}

impl Turn {
    /// The turn as a page holds it: its prompt and at most `MAX_TURN_ENTRIES - 1` of its newest
    /// other entries.
    fn into_history(self) -> AgentHistoryTurn {
        let (placed, finished) = self.entries();
        let mut entries: Vec<_> = placed.into_iter().map(|placed| placed.entry).collect();
        let truncated = entries.len() > MAX_TURN_ENTRIES;
        if truncated {
            entries.drain(1..entries.len() - (MAX_TURN_ENTRIES - 1));
        }
        AgentHistoryTurn {
            id: self.prompt.id,
            started_at: self.prompt.timestamp,
            finished,
            entries,
            truncated,
        }
    }
}

/// How many bytes a value takes as JSON.
fn json_size<T: serde::Serialize>(value: &T) -> usize {
    struct Count(usize);
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    // Plain data cannot fail to serialise, and counting cannot fail to write.
    let _ = serde_json::to_writer(&mut count, value);
    count.0
}

/// Names a turn: where its prompt is written, and which prompt it is.
struct Before {
    start: u64,
    fingerprint: u64,
}

impl Before {
    fn of(turn: &Turn) -> Self {
        Self {
            start: turn.start,
            fingerprint: fingerprint(&turn.prompt.id),
        }
    }

    fn encode(&self) -> String {
        format!("h:{:x}.{:x}", self.start, self.fingerprint)
    }

    fn decode(text: &str) -> Option<Self> {
        let (start, fingerprint) = text.strip_prefix("h:")?.split_once('.')?;
        Some(Self {
            start: u64::from_str_radix(start, 16).ok()?,
            fingerprint: u64::from_str_radix(fingerprint, 16).ok()?,
        })
    }

    /// How the prompt that opens the turn links to the rest of the transcript, or `None` when
    /// the transcript has no such prompt there.
    fn check<R: Read + Seek>(
        &self,
        format: TranscriptFormat,
        lines: &mut ReverseLines<R>,
    ) -> io::Result<Option<Link>> {
        let Some(bytes) = lines.line_at(self.start)? else {
            return Ok(None);
        };
        let parsed = parse(format, &bytes, false);
        Ok(match parsed.line {
            Line::Prompt(prompt) if fingerprint(&prompt.id) == self.fingerprint => parsed.link,
            _ => None,
        })
    }
}
