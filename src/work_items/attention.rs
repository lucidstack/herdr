//! Whether work items, and agents outside them, need you, and when that changes. Pure; no
//! I/O.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::api::schema::{AttentionInfo, AttentionKind};
use crate::detect::AgentState;

/// How long a finished agent's pane must be left alone before it needs you, so a turn
/// that ends while you are at the desk is not reported.
pub(crate) const FINISHED_QUIET_PERIOD: Duration = Duration::from_secs(120);

/// A need before it is tracked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Need {
    pub kind: AttentionKind,
    pub reason: String,
    pub pane_id: Option<String>,
}

impl Need {
    pub(crate) fn new(kind: AttentionKind, reason: impl Into<String>) -> Self {
        Self {
            kind,
            reason: reason.into(),
            pane_id: None,
        }
    }
}

/// One agent pane as the rules see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentSignal<'a> {
    pub pane_id: String,
    pub agent: &'a str,
    pub state: AgentState,
    /// What the agent reported while blocked, e.g. its permission prompt.
    pub blocked_message: Option<&'a str>,
    pub turn_finished_at: Option<Instant>,
    /// The `turn_finished_at` of a turn the user dismissed rather than typed to the pane.
    pub dismissed_turn: Option<Instant>,
    pub last_input_at: Option<Instant>,
}

/// What one agent means for you right now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AgentVerdict {
    pub need: Option<Need>,
    pub working: bool,
    /// When the agent's latest turn ended, while it sits idle after one, whether or not you
    /// dealt with it since.
    pub turn_finished_at: Option<Instant>,
    /// When the verdict may change without any new signal: a finished turn's quiet period
    /// ending.
    pub recheck_at: Option<Instant>,
}

impl AgentVerdict {
    /// Whether an agent finished a turn after `at`, a moment on the same monotonic clock as
    /// turn ends. The same moment is not after.
    pub(crate) fn took_turn_after(&self, at: Instant) -> bool {
        self.turn_finished_at.is_some_and(|ended| ended > at)
    }
}

/// Blocked: needs you. Idle after a turn: needs you once the pane was left alone for
/// `FINISHED_QUIET_PERIOD` since the turn ended; input after the turn ended, or dismissing
/// the turn, means you already dealt with it.
pub(crate) fn agent_verdict(signal: &AgentSignal<'_>, now: Instant) -> AgentVerdict {
    match signal.state {
        AgentState::Blocked => {
            let reason = signal
                .blocked_message
                .and_then(|message| message.lines().map(str::trim).find(|line| !line.is_empty()))
                .map_or_else(
                    || format!("{} is waiting for you", signal.agent),
                    str::to_string,
                );
            AgentVerdict {
                need: Some(Need {
                    kind: AttentionKind::Blocked,
                    reason,
                    pane_id: Some(signal.pane_id.clone()),
                }),
                ..AgentVerdict::default()
            }
        }
        AgentState::Working => AgentVerdict {
            working: true,
            ..AgentVerdict::default()
        },
        AgentState::Idle => {
            let Some(finished_at) = signal.turn_finished_at else {
                return AgentVerdict::default();
            };
            // Whatever you do about the turn, it was taken.
            let turn = AgentVerdict {
                turn_finished_at: Some(finished_at),
                ..AgentVerdict::default()
            };
            let dealt_with = signal.dismissed_turn == Some(finished_at)
                || signal
                    .last_input_at
                    .is_some_and(|input| input > finished_at);
            if dealt_with {
                return turn;
            }
            let due = finished_at + FINISHED_QUIET_PERIOD;
            if now < due {
                return AgentVerdict {
                    recheck_at: Some(due),
                    ..turn
                };
            }
            AgentVerdict {
                need: Some(Need {
                    kind: AttentionKind::Finished,
                    reason: format!("{} finished its turn", signal.agent),
                    pane_id: Some(signal.pane_id.clone()),
                }),
                ..turn
            }
        }
        AgentState::Unknown => AgentVerdict::default(),
    }
}

/// Every agent of one workspace folded together: the most urgent need, whether any agent is
/// working, and the latest turn any of them finished.
pub(crate) fn fold_verdicts(verdicts: impl IntoIterator<Item = AgentVerdict>) -> AgentVerdict {
    verdicts
        .into_iter()
        .fold(AgentVerdict::default(), |mut folded, verdict| {
            folded.working |= verdict.working;
            folded.recheck_at = earliest(folded.recheck_at, verdict.recheck_at);
            folded.need = most_urgent(folded.need.take(), verdict.need);
            folded.turn_finished_at = folded.turn_finished_at.max(verdict.turn_finished_at);
            folded
        })
}

/// What an item's own state says, besides its agents.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ItemSignals {
    /// Provisioning or a choice failed.
    pub failure: Option<Need>,
    /// Herdr is still doing something for the item: provisioning, or carrying out a choice.
    pub busy: bool,
    /// What the tracker asks of you, including items folded into this one.
    pub tracker: Option<Need>,
}

/// Blocked agents first, then failures, then finished agents. While an agent works or
/// Herdr is busy on the item, the tracker's asks wait: someone is already on it.
pub(crate) fn item_need(agents: &AgentVerdict, item: ItemSignals) -> Option<Need> {
    let agent_need = agents.need.as_ref();
    if let Some(need) = agent_need.filter(|need| need.kind == AttentionKind::Blocked) {
        return Some(need.clone());
    }
    if item.failure.is_some() {
        return item.failure;
    }
    if let Some(need) = agent_need {
        return Some(need.clone());
    }
    if agents.working || item.busy {
        return None;
    }
    item.tracker
}

pub(crate) fn most_urgent(a: Option<Need>, b: Option<Need>) -> Option<Need> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.kind < a.kind { b } else { a }),
        (a, b) => a.or(b),
    }
}

pub(crate) fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Subject {
    Item(String),
    /// An agent's pane outside every item.
    Pane(String),
}

/// Something that needs you now, with what an event about it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub need: Need,
    pub title: String,
    pub workspace_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    info: AttentionInfo,
    title: String,
    workspace_id: Option<String>,
}

/// A subject started or stopped needing you, or needs you for another kind of reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Transition {
    pub subject: Subject,
    pub title: String,
    pub workspace_id: Option<String>,
    /// `None` once it no longer needs you.
    pub attention: Option<AttentionInfo>,
}

/// The current needs, and when each started. Until the first update there is no baseline,
/// so needs found at start-up are recorded without transitions.
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    baseline: bool,
    entries: HashMap<Subject, Entry>,
}

impl Tracker {
    pub(crate) fn get(&self, subject: &Subject) -> Option<&AttentionInfo> {
        self.entries.get(subject).map(|entry| &entry.info)
    }

    /// Forgets the baseline; the next update only records the needs.
    pub(crate) fn reset(&mut self) {
        self.baseline = false;
    }

    /// Replaces the needs with `current`, every subject that needs you now. A need keeps
    /// its `since` while its kind stays the same, even if its reason changes. Transitions
    /// come in subject order; none on the first update.
    pub(crate) fn update(&mut self, current: HashMap<Subject, Candidate>, now_unix: u64) -> Update {
        let report = std::mem::replace(&mut self.baseline, true);
        let mut previous = std::mem::take(&mut self.entries);
        let mut update = Update::default();
        for (subject, candidate) in current {
            let old = previous.remove(&subject);
            let same_kind = old
                .as_ref()
                .is_some_and(|old| old.info.kind == candidate.need.kind);
            let since = match &old {
                Some(old) if same_kind => old.info.since,
                _ => now_unix,
            };
            let entry = Entry {
                info: AttentionInfo {
                    kind: candidate.need.kind,
                    reason: candidate.need.reason,
                    pane_id: candidate.need.pane_id,
                    since,
                },
                title: candidate.title,
                workspace_id: candidate.workspace_id,
            };
            if matches!(subject, Subject::Item(_))
                && old.as_ref().map(|old| &old.info) != Some(&entry.info)
            {
                update.items_changed = true;
            }
            if report && !same_kind {
                update.transitions.push(Transition {
                    subject: subject.clone(),
                    title: entry.title.clone(),
                    workspace_id: entry.workspace_id.clone(),
                    attention: Some(entry.info.clone()),
                });
            }
            self.entries.insert(subject, entry);
        }
        for (subject, entry) in previous {
            update.items_changed |= matches!(subject, Subject::Item(_));
            if report {
                update.transitions.push(Transition {
                    subject,
                    title: entry.title,
                    workspace_id: entry.workspace_id,
                    attention: None,
                });
            }
        }
        update.transitions.sort_by(|a, b| a.subject.cmp(&b.subject));
        update
    }
}

#[derive(Debug, Default)]
pub(crate) struct Update {
    pub transitions: Vec<Transition>,
    /// Whether any item's attention changed, reason included.
    pub items_changed: bool,
}

/// When attention must be worked out again: something it depends on changed, or a
/// finished turn's quiet period ends.
#[derive(Debug)]
pub(crate) struct Schedule {
    dirty: bool,
    revision: u64,
    recheck_at: Option<Instant>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            dirty: true,
            revision: 0,
            recheck_at: None,
        }
    }
}

impl Schedule {
    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// Whether to work it out now, given the work items' current `revision`.
    pub(crate) fn is_due(&self, now: Instant, revision: u64) -> bool {
        self.dirty || revision != self.revision || self.recheck_at.is_some_and(|at| at <= now)
    }

    /// Records a finished pass: `revision` is the work items' revision after it.
    pub(crate) fn done(&mut self, revision: u64, recheck_at: Option<Instant>) {
        self.dirty = false;
        self.revision = revision;
        self.recheck_at = recheck_at;
    }

    pub(crate) fn recheck_at(&self) -> Option<Instant> {
        self.recheck_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(state: AgentState) -> AgentSignal<'static> {
        AgentSignal {
            pane_id: "w1-1".into(),
            agent: "claude",
            state,
            blocked_message: None,
            turn_finished_at: None,
            dismissed_turn: None,
            last_input_at: None,
        }
    }

    fn candidate(kind: AttentionKind, reason: &str) -> Candidate {
        Candidate {
            need: Need::new(kind, reason),
            title: "TECH-1".into(),
            workspace_id: None,
        }
    }

    fn item(key: &str) -> Subject {
        Subject::Item(key.into())
    }

    #[test]
    fn blocked_agent_needs_you_with_the_first_line_of_its_prompt() {
        let mut blocked = signal(AgentState::Blocked);
        blocked.blocked_message = Some("\n  Allow running bin/rails db:migrate?\nYes / No");
        let need = agent_verdict(&blocked, Instant::now())
            .need
            .expect("blocked needs you");
        assert_eq!(need.kind, AttentionKind::Blocked);
        assert_eq!(need.reason, "Allow running bin/rails db:migrate?");
        assert_eq!(need.pane_id.as_deref(), Some("w1-1"));

        let silent = agent_verdict(&signal(AgentState::Blocked), Instant::now());
        assert_eq!(silent.need.unwrap().reason, "claude is waiting for you");
    }

    #[test]
    fn finished_turn_needs_you_only_after_the_pane_was_left_alone() {
        let finished_at = Instant::now();
        let mut idle = signal(AgentState::Idle);
        idle.turn_finished_at = Some(finished_at);
        idle.last_input_at = Some(finished_at - Duration::from_secs(5));

        let early = agent_verdict(&idle, finished_at + Duration::from_secs(60));
        assert_eq!(early.need, None);
        assert_eq!(early.recheck_at, Some(finished_at + FINISHED_QUIET_PERIOD));

        let due = agent_verdict(&idle, finished_at + FINISHED_QUIET_PERIOD);
        assert_eq!(
            due.need.map(|need| need.kind),
            Some(AttentionKind::Finished)
        );

        idle.last_input_at = Some(finished_at + Duration::from_secs(30));
        let touched = agent_verdict(&idle, finished_at + Duration::from_secs(600));
        assert_eq!(touched.need, None);
        assert_eq!(touched.recheck_at, None);
    }

    #[test]
    fn idle_agent_that_never_finished_a_turn_does_not_need_you() {
        let verdict = agent_verdict(&signal(AgentState::Idle), Instant::now());
        assert_eq!(verdict, AgentVerdict::default());
    }

    #[test]
    fn dismissing_a_finished_turn_deals_with_it_as_input_after_it_would() {
        let finished_at = Instant::now();
        let mut idle = signal(AgentState::Idle);
        idle.turn_finished_at = Some(finished_at);
        let late = finished_at + FINISHED_QUIET_PERIOD + Duration::from_secs(1);
        assert_eq!(
            agent_verdict(&idle, late).need.map(|need| need.kind),
            Some(AttentionKind::Finished)
        );

        idle.dismissed_turn = Some(finished_at);
        let dismissed = agent_verdict(&idle, late);
        assert_eq!(dismissed.need, None);
        assert_eq!(dismissed.recheck_at, None);

        // The next turn is its own: the earlier dismissal does not cover it.
        let next = finished_at + Duration::from_secs(300);
        idle.turn_finished_at = Some(next);
        assert_eq!(
            agent_verdict(&idle, next + FINISHED_QUIET_PERIOD)
                .need
                .map(|need| need.kind),
            Some(AttentionKind::Finished)
        );
    }

    #[test]
    fn a_turn_stays_taken_whether_or_not_it_needs_you() {
        let finished_at = Instant::now();
        let seconds = Duration::from_secs;
        let turn = |signal: &AgentSignal<'_>, now| agent_verdict(signal, now).turn_finished_at;
        let mut idle = signal(AgentState::Idle);
        idle.turn_finished_at = Some(finished_at);

        // Within the quiet period, once it is due, and after you dealt with it.
        assert_eq!(turn(&idle, finished_at + seconds(10)), Some(finished_at));
        assert_eq!(turn(&idle, finished_at + seconds(300)), Some(finished_at));
        idle.last_input_at = Some(finished_at + seconds(1));
        assert_eq!(turn(&idle, finished_at + seconds(300)), Some(finished_at));

        // A working agent is on a turn, not after one, whatever turn it finished before.
        let mut working = signal(AgentState::Working);
        working.turn_finished_at = Some(finished_at);
        assert_eq!(turn(&working, finished_at + seconds(300)), None);
    }

    #[test]
    fn a_turn_is_after_a_moment_only_when_it_ended_later() {
        // Ahead of now, so moments before it are safe to make.
        let failed_at = Instant::now() + Duration::from_secs(60);
        let ended = |at: Instant| AgentVerdict {
            turn_finished_at: Some(at),
            ..AgentVerdict::default()
        };
        assert!(ended(failed_at + Duration::from_millis(1)).took_turn_after(failed_at));
        assert!(
            !ended(failed_at).took_turn_after(failed_at),
            "the same moment is not after"
        );
        assert!(!ended(failed_at - Duration::from_secs(1)).took_turn_after(failed_at));
        assert!(!AgentVerdict::default().took_turn_after(failed_at));
    }

    #[test]
    fn workspace_remembers_its_latest_turn() {
        let now = Instant::now();
        let ended = |at| AgentVerdict {
            turn_finished_at: Some(at),
            ..AgentVerdict::default()
        };
        let latest = now + Duration::from_secs(70);
        let folded = fold_verdicts([
            ended(now),
            ended(latest),
            AgentVerdict::default(),
            ended(now + Duration::from_secs(5)),
        ]);
        assert_eq!(folded.turn_finished_at, Some(latest));
    }

    #[test]
    fn workspace_takes_its_most_urgent_agent_and_earliest_recheck() {
        let now = Instant::now();
        let finished = AgentVerdict {
            need: Some(Need::new(AttentionKind::Finished, "done")),
            ..AgentVerdict::default()
        };
        let blocked = AgentVerdict {
            need: Some(Need::new(AttentionKind::Blocked, "allow?")),
            ..AgentVerdict::default()
        };
        let working = AgentVerdict {
            working: true,
            recheck_at: Some(now + Duration::from_secs(9)),
            ..AgentVerdict::default()
        };
        let later = AgentVerdict {
            recheck_at: Some(now + Duration::from_secs(99)),
            ..AgentVerdict::default()
        };
        let folded = fold_verdicts([finished, working, blocked, later]);
        assert_eq!(folded.need.unwrap().kind, AttentionKind::Blocked);
        assert!(folded.working);
        assert_eq!(folded.recheck_at, Some(now + Duration::from_secs(9)));
    }

    #[test]
    fn blocked_agent_outranks_a_failure_which_outranks_a_finished_agent() {
        let failure = || ItemSignals {
            failure: Some(Need::new(AttentionKind::Failed, "checkout failed")),
            ..ItemSignals::default()
        };
        let agents = |kind| AgentVerdict {
            need: Some(Need::new(kind, "agent")),
            ..AgentVerdict::default()
        };
        assert_eq!(
            item_need(&agents(AttentionKind::Blocked), failure()).map(|need| need.kind),
            Some(AttentionKind::Blocked)
        );
        assert_eq!(
            item_need(&agents(AttentionKind::Finished), failure()).map(|need| need.kind),
            Some(AttentionKind::Failed)
        );
    }

    #[test]
    fn tracker_asks_wait_while_an_agent_works_or_herdr_is_busy() {
        let tracker = || ItemSignals {
            tracker: Some(Need::new(AttentionKind::ChangesRequested, "changes")),
            ..ItemSignals::default()
        };
        let working = AgentVerdict {
            working: true,
            ..AgentVerdict::default()
        };
        assert_eq!(item_need(&working, tracker()), None);
        let busy = ItemSignals {
            busy: true,
            ..tracker()
        };
        assert_eq!(item_need(&AgentVerdict::default(), busy), None);
        assert_eq!(
            item_need(&AgentVerdict::default(), tracker()).map(|need| need.kind),
            Some(AttentionKind::ChangesRequested)
        );
    }

    #[test]
    fn needs_found_at_start_up_are_recorded_without_transitions() {
        let mut tracker = Tracker::default();
        let transitions = tracker
            .update(
                HashMap::from([(item("a"), candidate(AttentionKind::New, "Review requested"))]),
                10,
            )
            .transitions;
        assert!(transitions.is_empty());
        assert_eq!(tracker.get(&item("a")).map(|info| info.since), Some(10));
    }

    #[test]
    fn transitions_report_entering_leaving_and_kind_changes_but_not_reason_changes() {
        let mut tracker = Tracker::default();
        tracker.update(
            HashMap::from([
                (item("kept"), candidate(AttentionKind::Blocked, "allow a?")),
                (item("left"), candidate(AttentionKind::New, "new")),
                (item("changed"), candidate(AttentionKind::New, "new")),
            ]),
            10,
        );
        let transitions = tracker
            .update(
                HashMap::from([
                    (item("kept"), candidate(AttentionKind::Blocked, "allow b?")),
                    (
                        item("changed"),
                        candidate(AttentionKind::ReadyToMerge, "merge"),
                    ),
                    (item("entered"), candidate(AttentionKind::Failed, "failed")),
                ]),
                20,
            )
            .transitions;
        let summary: Vec<_> = transitions
            .iter()
            .map(|t| {
                (
                    t.subject.clone(),
                    t.attention.as_ref().map(|a| (a.kind, a.since)),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (item("changed"), Some((AttentionKind::ReadyToMerge, 20))),
                (item("entered"), Some((AttentionKind::Failed, 20))),
                (item("left"), None),
            ]
        );
        let kept = tracker.get(&item("kept")).expect("still blocked");
        assert_eq!((kept.reason.as_str(), kept.since), ("allow b?", 10));
        assert_eq!(tracker.get(&item("left")), None);
    }

    #[test]
    fn leaving_carries_the_last_title_and_workspace() {
        let mut tracker = Tracker::default();
        let mut blocked = candidate(AttentionKind::Blocked, "allow?");
        blocked.title = "claude · api".into();
        blocked.workspace_id = Some("w1".into());
        tracker.update(HashMap::from([(Subject::Pane("w1-1".into()), blocked)]), 1);
        let transitions = tracker.update(HashMap::new(), 2).transitions;
        assert_eq!(
            transitions,
            vec![Transition {
                subject: Subject::Pane("w1-1".into()),
                title: "claude · api".into(),
                workspace_id: Some("w1".into()),
                attention: None,
            }]
        );
    }

    #[test]
    fn reset_rebaselines_without_transitions() {
        let mut tracker = Tracker::default();
        tracker.update(HashMap::new(), 1);
        tracker.reset();
        let transitions = tracker
            .update(
                HashMap::from([(item("a"), candidate(AttentionKind::New, "new"))]),
                2,
            )
            .transitions;
        assert!(transitions.is_empty());
    }

    #[test]
    fn items_change_on_a_new_reason_but_not_when_only_agents_outside_items_change() {
        let mut tracker = Tracker::default();
        let blocked =
            |reason| HashMap::from([(item("a"), candidate(AttentionKind::Blocked, reason))]);
        tracker.update(blocked("allow a?"), 1);

        let same = tracker.update(blocked("allow a?"), 2);
        assert!(!same.items_changed);

        let reworded = tracker.update(blocked("allow b?"), 3);
        assert!(reworded.items_changed);
        assert!(reworded.transitions.is_empty());

        let mut with_pane = blocked("allow b?");
        with_pane.insert(
            Subject::Pane("w2-1".into()),
            candidate(AttentionKind::Finished, "done"),
        );
        let pane_only = tracker.update(with_pane, 4);
        assert!(!pane_only.items_changed);
        assert_eq!(pane_only.transitions.len(), 1);
    }

    #[test]
    fn schedule_is_due_when_dirty_behind_or_at_its_recheck() {
        let now = Instant::now();
        let mut schedule = Schedule::default();
        assert!(schedule.is_due(now, 0));
        schedule.done(3, Some(now + Duration::from_secs(5)));
        assert!(!schedule.is_due(now, 3));
        assert!(schedule.is_due(now, 4));
        assert!(schedule.is_due(now + Duration::from_secs(5), 3));
        schedule.mark_dirty();
        assert!(schedule.is_due(now, 3));
    }
}
