//! Walks a transcript backwards, one turn at a time: the prompt that opens a turn and the
//! lines after it, up to the next prompt.
//!
//! The walk either takes every line in file order, which is all the current turn needs and
//! keeps the lines of parallel tool calls, or follows the active branch: from the newest
//! entry it goes parent by parent and skips every line that is not on the way, so the turns
//! of an abandoned branch (a rewound Claude Code session, a branched omp session) never
//! show. Parents are written before their children, so one backward pass sees the whole
//! branch.
//!
//! Claude Code writes the lines of a response that calls tools in parallel as siblings of
//! one another, and the branch goes on through only one of them, so a line off to the side
//! still counts when it is a tool result, which answers a call whether it hangs on the
//! branch or not, or a line of a response the branch goes through.
//!
//! A session can open with a message that is not the user's, such as the context of a
//! handoff, followed by the agent's work. That message opens the session's first turn: going
//! back, a context line is held until the walk either meets an older line of the conversation,
//! which makes it a later one that does not count, or runs out, which makes it the opening.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Seek};

use super::{parse, Line, Link, Prompt, Turn};
use crate::transcript::{ReverseLines, TranscriptFormat};

/// What the walk found next, going back.
pub(super) enum Step {
    /// A line of the turn being read.
    Line(u64, Line),
    /// The prompt that opens it, with the offset of its line.
    Prompt(u64, Prompt),
    /// A message the session gave the model that is not the user's. It opens the first turn
    /// when nothing older is in the conversation.
    Context(u64, Prompt),
    /// Nothing older is left.
    End,
    /// The walk went back past the floor it was given.
    Far,
}

pub(super) struct Scan<R> {
    format: TranscriptFormat,
    lines: ReverseLines<R>,
    thinking: bool,
    /// The active branch to follow, or `None` to take every line.
    branch: Option<Branch>,
    /// Offset just past the last complete line of the file.
    end: u64,
    /// Whether the next line read is the file's last.
    first: bool,
    /// Offset of the oldest line read so far.
    at: u64,
    /// Whether the line after the one being examined, bookkeeping aside, is what a local
    /// slash command printed.
    local_output_next: bool,
    /// Lines the branch brought in that are yet to be handed out, the next one last.
    ready: Vec<(u64, Line)>,
}

impl<R: Read + Seek> Scan<R> {
    fn new(
        format: TranscriptFormat,
        lines: ReverseLines<R>,
        thinking: bool,
        branch: Option<Branch>,
    ) -> Self {
        let len = lines.len;
        Self {
            format,
            lines,
            thinking,
            branch,
            end: len,
            first: true,
            at: len,
            local_output_next: false,
            ready: Vec::new(),
        }
    }

    /// From the end of the file, taking every line in file order.
    pub(super) fn in_file_order(
        format: TranscriptFormat,
        lines: ReverseLines<R>,
        thinking: bool,
    ) -> Self {
        Self::new(format, lines, thinking, None)
    }

    /// From the end of the file, following the active branch.
    pub(super) fn on_branch(
        format: TranscriptFormat,
        lines: ReverseLines<R>,
        thinking: bool,
    ) -> Self {
        Self::new(format, lines, thinking, Some(Branch::new(Wanted::Newest)))
    }

    /// From the line before the one at `offset`, on the branch that goes through `parent`.
    pub(super) fn resume(
        format: TranscriptFormat,
        mut lines: ReverseLines<R>,
        thinking: bool,
        offset: u64,
        parent: Option<String>,
    ) -> Self {
        lines.restart_at(offset);
        let mut branch = Branch::new(parent.map_or(Wanted::Newest, Wanted::Entry));
        branch.member_at = Some(offset);
        let mut scan = Self::new(format, lines, thinking, Some(branch));
        scan.first = false;
        scan.at = offset;
        scan
    }

    /// The next older turn, or `None` when the transcript has no more.
    pub(super) fn next_turn(&mut self) -> io::Result<Option<Turn>> {
        // The lines after the prompt, newest first.
        let mut collected = Vec::new();
        // The oldest context line met so far, and how many lines had been collected then: the
        // session opened with it when the walk runs out without collecting more.
        let mut opening = None;
        loop {
            match self.step(0)? {
                Step::Line(offset, line) => collected.push((offset, line)),
                Step::Context(offset, context) => {
                    opening = Some((offset, context, collected.len()))
                }
                Step::Prompt(offset, prompt) => {
                    return Ok(Some(self.turn(offset, prompt, collected)))
                }
                Step::End => {
                    return Ok(opening
                        .filter(|(_, _, seen)| *seen == collected.len())
                        .map(|(offset, context, _)| self.turn(offset, context, collected)));
                }
                Step::Far => return Ok(None),
            }
        }
    }

    /// The turn that `opener` starts, from its lines, newest first.
    fn turn(&self, offset: u64, opener: Prompt, collected: Vec<(u64, Line)>) -> Turn {
        let mut turn = Turn::new(offset, self.end, opener);
        for (offset, line) in collected.into_iter().rev() {
            turn.push(offset, line);
        }
        turn
    }

    /// Whether the transcript has a turn older than the ones read. It looks back at most
    /// `reach` bytes; past that it takes the answer to be yes, so that a huge turn is not
    /// read in full only to learn that it is there.
    pub(super) fn has_older(&mut self, reach: u64) -> io::Result<bool> {
        let floor = self.at.saturating_sub(reach);
        // Whether a context line was met with nothing older after it.
        let mut opening = false;
        loop {
            match self.step(floor)? {
                Step::Line(..) => opening = false,
                Step::Context(..) => opening = true,
                Step::Prompt(..) | Step::Far => return Ok(true),
                Step::End => return Ok(opening),
            }
        }
    }

    /// The next line that matters going back, or the prompt that opens the turn. A line at an
    /// offset below `floor` ends the walk.
    fn step(&mut self, floor: u64) -> io::Result<Step> {
        loop {
            if let Some((offset, line)) = self.ready.pop() {
                self.local_output_next = false;
                return Ok(Step::Line(offset, line));
            }
            let Some(line) = self.lines.next_with_offset() else {
                // A chain that leads to an entry the file does not have ends where it did have
                // one, and the walk goes on from there as it does past the first entry of a
                // chain.
                if let Some(at) = self.branch.as_mut().and_then(Branch::lost) {
                    self.lines.restart_at(at);
                    continue;
                }
                return Ok(Step::End);
            };
            let (offset, bytes) = line?;
            self.at = offset;
            if std::mem::take(&mut self.first) && offset + bytes.len() as u64 == self.lines.len {
                // The writer has not ended this line yet. Leave it for the next read, which
                // sees it whole.
                self.end = offset;
            }
            if offset < floor {
                return Ok(Step::Far);
            }
            let parsed = parse(self.format, &bytes, self.thinking);
            let line = match &mut self.branch {
                None => parsed.line,
                Some(branch) => match branch.admit(offset, parsed.link.as_ref(), parsed.line) {
                    Verdict::Take(line) => line,
                    Verdict::Adopt(lines) => {
                        self.ready = lines;
                        continue;
                    }
                    Verdict::Skip => continue,
                },
            };
            match line {
                // A local slash command prints its result and the model never answers it:
                // what it printed follows it directly, and the turn is an earlier one.
                Line::Prompt(prompt) if prompt.command && self.local_output_next => {
                    self.local_output_next = false;
                }
                Line::Prompt(prompt) => {
                    self.local_output_next = false;
                    if let Some(branch) = &mut self.branch {
                        branch.turn_over();
                    }
                    return Ok(Step::Prompt(offset, prompt));
                }
                // Whether it opens a turn depends on what is older, which the caller sees next.
                Line::Context(context) => {
                    self.local_output_next = false;
                    return Ok(Step::Context(offset, context));
                }
                line @ (Line::Assistant(_)
                | Line::ToolResults(_)
                | Line::MidPrompt(_)
                | Line::Compaction(_)
                | Line::Interrupt) => {
                    self.local_output_next = false;
                    return Ok(Step::Line(offset, line));
                }
                Line::LocalOutput => self.local_output_next = true,
                Line::Other => {}
            }
        }
    }
}

/// The active branch of a transcript, followed from its newest entry.
struct Branch {
    wanted: Wanted,
    /// The responses, by Claude Code's message id, that the branch goes through in the turn
    /// being read.
    responses: HashSet<String>,
    /// Lines of responses that hang off the side of the branch, until the branch is met going
    /// through their response. They come before the line that brings them in, so they wait.
    aside: HashMap<String, Vec<(u64, Line)>>,
    /// Where the oldest entry on the branch so far is written.
    member_at: Option<u64>,
}

enum Wanted {
    /// The newest entry that can end a branch has not been met yet: the walk is at the end of
    /// the transcript, or at the first entry of a branch, which is as far as a chain goes.
    Newest,
    /// The next entry on the branch is this one.
    Entry(String),
}

/// What the branch makes of a line.
enum Verdict {
    /// The line counts.
    Take(Line),
    /// Lines that count, the one to hand out next last: the line, which is the oldest, and
    /// the lines of its response that were set aside.
    Adopt(Vec<(u64, Line)>),
    Skip,
}

impl Branch {
    fn new(wanted: Wanted) -> Self {
        Self {
            wanted,
            responses: HashSet::new(),
            aside: HashMap::new(),
            member_at: None,
        }
    }

    /// When the walk ran out of lines looking for an entry that is not there, the place to go
    /// on from, looking for the newest entry before it. What the search set aside is
    /// forgotten, as the lines are read again.
    fn lost(&mut self) -> Option<u64> {
        let at = self.member_at.take()?;
        if !matches!(self.wanted, Wanted::Entry(_)) {
            return None;
        }
        self.wanted = Wanted::Newest;
        self.turn_over();
        Some(at)
    }

    /// Whether the line counts: it is the next entry on the branch, which the walk then goes
    /// on from; or a tool result, wherever it hangs; or a line of a response the branch goes
    /// through.
    ///
    /// A chain that ends does not end the walk. Claude Code starts a second chain where it
    /// has none to continue, for the instruction a teammate agent is given before its session
    /// begins, say, and the older chain is a segment of the conversation of its own.
    fn admit(&mut self, offset: u64, link: Option<&Link>, line: Line) -> Verdict {
        let Some(link) = link else {
            return Verdict::Skip;
        };
        let response = match &line {
            Line::Assistant(assistant) => assistant.response.clone(),
            _ => None,
        };
        let on_branch = match &self.wanted {
            Wanted::Newest => link.tip,
            Wanted::Entry(id) => *id == link.id,
        };
        if on_branch {
            self.member_at = Some(offset);
            self.wanted = link.parent.clone().map_or(Wanted::Newest, Wanted::Entry);
            if let Some(response) = response {
                if let Some(mut aside) = self.aside.remove(&response) {
                    // Newest first as they were met, and this line is older than all of them.
                    aside.push((offset, line));
                    aside.reverse();
                    self.responses.insert(response);
                    return Verdict::Adopt(aside);
                }
                self.responses.insert(response);
            }
            return Verdict::Take(line);
        }
        if link.result {
            return Verdict::Take(line);
        }
        match response {
            Some(response) if self.responses.contains(&response) => Verdict::Take(line),
            Some(response) => {
                self.aside.entry(response).or_default().push((offset, line));
                Verdict::Skip
            }
            None => Verdict::Skip,
        }
    }

    /// A turn is read: what was set aside for it will not be needed.
    fn turn_over(&mut self) {
        self.responses.clear();
        self.aside.clear();
    }
}
