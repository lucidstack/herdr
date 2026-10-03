//! The shell commands an agent is running right now, for the rule that an agent stuck on one
//! command needs you.
//!
//! The same backward pass `activity` makes over the lines of the current turn, with the same
//! parsers, so a call has the id, the Shell kind and the Running status `agent.activity` gives
//! it, but it keeps only the calls that have no result and never builds the feed. It also gives
//! up `REACH` bytes before the end of the file whatever the turn holds: it runs for every
//! working agent once a minute, and a turn of tens of megabytes must not cost that much each
//! time. A command that began more than `REACH` bytes of transcript ago is not found, which is
//! never taken for stuck.

use std::collections::HashSet;
use std::io::{self, Read, Seek};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::scan::Scan;
use super::{Block, Line};
use crate::api::schema::AgentActivityToolKind;
use crate::transcript::{ReverseLines, TranscriptFormat, READ_CHUNK};

/// How far back from the end of the transcript a running command is looked for.
const REACH: u64 = 4 * 1024 * 1024;

/// A shell command of the current turn that has no result yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningShellCall {
    /// The id of the call, which is the id of its entry in `agent.activity`.
    pub call_id: String,
    /// When the transcript wrote the call. `None` when the line has no time, or one that is
    /// not RFC 3339.
    pub started_at: Option<SystemTime>,
    /// What the command is: its first line, or what the call says it does when it has none.
    pub label: String,
}

/// The shell calls of the transcript's current turn that have no result yet, oldest first.
pub fn running_shell_calls(
    format: TranscriptFormat,
    path: &Path,
) -> io::Result<Vec<RunningShellCall>> {
    let lines = ReverseLines::new(std::fs::File::open(path)?, READ_CHUNK)?;
    read_running(format, lines, REACH)
}

pub(super) fn read_running<R: Read + Seek>(
    format: TranscriptFormat,
    lines: ReverseLines<R>,
    reach: u64,
) -> io::Result<Vec<RunningShellCall>> {
    // Going back, a result comes before the call it answers.
    let mut answered = HashSet::new();
    // Newest first, as they are read.
    let mut running = Vec::new();
    Scan::in_file_order(format, lines, false).visit_turn(reach, |line| match line {
        Line::ToolResults(results) => {
            answered.extend(results.into_iter().map(|result| result.call));
        }
        Line::Assistant(assistant) => {
            let started_at = assistant.timestamp.as_deref().and_then(parse_timestamp);
            for block in assistant.blocks.into_iter().rev() {
                let Block::Call(call) = block else {
                    continue;
                };
                if call.tool.kind == AgentActivityToolKind::Shell && !answered.contains(&call.id) {
                    running.push(RunningShellCall {
                        label: call.tool.target.unwrap_or(call.tool.summary),
                        call_id: call.id,
                        started_at,
                    });
                }
            }
        }
        _ => {}
    })?;
    running.reverse();
    Ok(running)
}

/// The moment an RFC 3339 timestamp names. Nothing is taken from one before 1970 or too far
/// ahead to represent, which no transcript was written at.
fn parse_timestamp(text: &str) -> Option<SystemTime> {
    let at = OffsetDateTime::parse(text, &Rfc3339).ok()?;
    let seconds = u64::try_from(at.unix_timestamp()).ok()?;
    UNIX_EPOCH.checked_add(Duration::new(seconds, at.nanosecond()))
}
