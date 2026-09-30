//! Notes that keep an agent up to date with what Herdr did outside its session, such as
//! marking its pull request ready for review from another client. The agent's integration
//! takes them with `agent.notes.take` when `agent.notes_added` tells it there are some,
//! and passes them to the agent: as steering while it works, or as context for its next
//! turn.

use crate::api::schema::{EventData, EventEnvelope, EventKind};

use super::terminal_targets::TerminalTarget;
use super::App;

/// Longest note accepted, in characters.
pub(super) const MAX_AGENT_NOTE_CHARS: usize = 2000;

impl App {
    /// Queues `text` for every agent in the workspace and tells their integrations.
    pub(super) fn note_for_workspace_agents(&mut self, workspace_id: &str, text: &str) {
        let Some(ws_idx) = self.parse_workspace_id(workspace_id) else {
            return;
        };
        for target in self.agent_targets_in_workspace(ws_idx) {
            self.note_for_agent(&target, text);
        }
    }

    /// Queues `text` for the agent at `target` and tells its integration.
    pub(super) fn note_for_agent(&mut self, target: &TerminalTarget, text: &str) {
        let Some(terminal) = self
            .state
            .terminals
            .values_mut()
            .find(|terminal| terminal.id.to_string() == target.terminal_id)
        else {
            return;
        };
        terminal.push_agent_note(text.to_string(), crate::work_items::unix_now());
        let Some(pane_id) = self.public_pane_id(target.ws_idx, target.pane_id) else {
            return;
        };
        let workspace_id = self.state.workspaces[target.ws_idx].id.clone();
        self.emit_event(EventEnvelope {
            event: EventKind::AgentNotesAdded,
            data: EventData::AgentNotesAdded {
                pane_id,
                workspace_id,
            },
        });
    }
}

/// The note an agent gets after Herdr carried out a choice on its item, e.g. "Done outside
/// this session: #11910 is ready for review (TECH-2072 · Add the flag)".
pub(super) fn action_note(outcome: &str, item_key: &str, item_title: &str) -> String {
    format!(
        "[Herdr] Done outside this session, at the user's request: {outcome} \
         ({item_key} · {item_title}). What you knew about this before may be out of date."
    )
}
