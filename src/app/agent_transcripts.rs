//! Finds the transcripts of Claude sessions that Herdr knows only by id, because their
//! integration never reported the file: sessions adopted before Herdr recorded transcript
//! paths, or whose session report omitted it.
//!
//! The search reads the agent's config directory, so it runs on a background thread and
//! only when a session is reported, restored, or asked about through the API, never from
//! view or detection work. A found transcript is recorded as if the integration had
//! reported it, so it persists and the search does not repeat.

use std::path::PathBuf;
use std::time::Instant;

use super::App;
use crate::events::{AgentTranscriptLookup, AppEvent};
use crate::layout::PaneId;

struct TranscriptSearch {
    pane_id: PaneId,
    terminal_id: crate::terminal::TerminalId,
    session_ref: crate::agent_resume::AgentSessionRef,
    cwd: Option<PathBuf>,
}

impl App {
    /// Searches for the missing transcript of every pane's agent session.
    pub(crate) fn look_up_missing_agent_transcripts(&mut self) {
        let panes: Vec<(usize, PaneId)> = self
            .state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(ws_idx, ws)| {
                ws.tabs
                    .iter()
                    .flat_map(|tab| tab.layout.pane_ids())
                    .map(move |pane_id| (ws_idx, pane_id))
            })
            .collect();
        self.look_up_agent_transcripts(panes);
    }

    /// Searches for the missing transcript of one pane's agent session.
    pub(crate) fn look_up_agent_transcript(&mut self, ws_idx: usize, pane_id: PaneId) {
        self.look_up_agent_transcripts([(ws_idx, pane_id)]);
    }

    fn look_up_agent_transcripts(&mut self, panes: impl IntoIterator<Item = (usize, PaneId)>) {
        let now = Instant::now();
        let mut searches = Vec::new();
        for (ws_idx, pane_id) in panes {
            let Some(tab) = self.state.workspaces.get(ws_idx).and_then(|ws| {
                ws.find_tab_index_for_pane(pane_id)
                    .and_then(|tab_idx| ws.tabs.get(tab_idx))
            }) else {
                continue;
            };
            let Some(terminal_id) = tab.terminal_id(pane_id).cloned() else {
                continue;
            };
            let Some(session_ref) = self
                .state
                .terminals
                .get_mut(&terminal_id)
                .and_then(|terminal| terminal.begin_agent_transcript_lookup(now))
            else {
                continue;
            };
            let cwd = tab.cwd_for_pane(pane_id, &self.state.terminals, &self.terminal_runtimes);
            searches.push(TranscriptSearch {
                pane_id,
                terminal_id,
                session_ref,
                cwd,
            });
        }
        if searches.is_empty() {
            return;
        }
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let config_dir = crate::integration::claude_dir();
            let lookups = searches
                .into_iter()
                .map(|search| {
                    let path = config_dir
                        .as_deref()
                        .ok()
                        .and_then(|config_dir| {
                            crate::transcript::find_claude_transcript(
                                config_dir,
                                &search.session_ref.value,
                                search.cwd.as_deref(),
                            )
                        })
                        .and_then(|path| path.into_os_string().into_string().ok());
                    AgentTranscriptLookup {
                        pane_id: search.pane_id,
                        terminal_id: search.terminal_id,
                        session_ref: search.session_ref,
                        path,
                    }
                })
                .collect();
            let _ = event_tx.blocking_send(AppEvent::AgentTranscriptsLookedUp(lookups));
        });
    }

    pub(super) fn finish_agent_transcript_lookups(&mut self, lookups: Vec<AgentTranscriptLookup>) {
        let now = Instant::now();
        for lookup in lookups {
            let recorded = self
                .state
                .terminals
                .get_mut(&lookup.terminal_id)
                .is_some_and(|terminal| {
                    terminal.finish_agent_transcript_lookup(&lookup.session_ref, lookup.path, now)
                });
            if !recorded {
                continue;
            }
            self.state.mark_session_dirty();
            let attached_ws_idx = self
                .find_pane(lookup.pane_id)
                .filter(|(_, pane)| pane.attached_terminal_id == lookup.terminal_id)
                .map(|(ws_idx, _)| ws_idx);
            if let Some(ws_idx) = attached_ws_idx {
                self.emit_pane_updated(ws_idx, lookup.pane_id);
            }
        }
    }
}
