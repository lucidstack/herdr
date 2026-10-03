//! Reads what each working agent is running, for the attention rule that an agent stuck on
//! one command needs you.
//!
//! Only an agent's transcript says what it is running: the hooks that report its state say
//! nothing of its commands. Reading the tail of a transcript is file I/O and parsing, so it
//! runs on a background thread, once a minute and only while an agent is working, never from
//! view or detection work. What a read finds is kept until the next one; the attention rules
//! apply the stuck period to it themselves. An agent whose transcript cannot be read, or shows
//! nothing running, has no running command, which is never stuck.

use std::path::Path;
use std::time::{Duration, Instant};

use super::App;
use crate::detect::AgentState;
use crate::events::{AppEvent, RunningCommandRead};
use crate::terminal::TerminalId;
use crate::transcript::TranscriptFormat;
use crate::work_items::attention::RunningCommand;

/// How often the transcripts of working agents are read: as often as the shortest stuck period
/// `work_items.stuck_after_seconds` allows.
const CHECK_INTERVAL: Duration = Duration::from_secs(60);

/// The transcript of one working agent.
struct Transcript {
    terminal_id: TerminalId,
    format: TranscriptFormat,
    path: String,
}

impl App {
    /// Reads the transcripts of the working agents on a background thread when the minute since
    /// the last read is over. The schedule runs only while an agent works, and only while work
    /// items are enabled, as attention is not worked out otherwise.
    pub(crate) fn run_running_command_check(&mut self, now: Instant) {
        let working = self.work_items.is_enabled()
            && self
                .state
                .terminals
                .values()
                .any(|terminal| terminal.state == AgentState::Working);
        if !working {
            self.running_command_check_at = None;
            self.running_commands.clear();
            return;
        }
        let due = *self
            .running_command_check_at
            .get_or_insert(now + CHECK_INTERVAL);
        if now < due {
            return;
        }
        // Whether or not this one starts, the next is a minute away: never left due.
        self.running_command_check_at = Some(now + CHECK_INTERVAL);
        if self.running_command_check_in_flight {
            return;
        }
        let transcripts = self.working_agent_transcripts();
        // What an earlier read found for an agent that stopped working, or has no transcript
        // to read, is not its own any more.
        self.running_commands.retain(|terminal_id, _| {
            transcripts
                .iter()
                .any(|transcript| transcript.terminal_id == *terminal_id)
        });
        if transcripts.is_empty() {
            return;
        }
        self.running_command_check_in_flight = true;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let reads = transcripts.into_iter().map(read_running_command).collect();
            let _ = event_tx.blocking_send(AppEvent::RunningCommandsRead(reads));
        });
    }

    /// The transcript of every agent that is working now, where Herdr has a reader for its
    /// format and knows the file: the session's agent owns the format, whatever the pane is
    /// labelled, as `agent.activity` has it.
    fn working_agent_transcripts(&self) -> Vec<Transcript> {
        self.state
            .terminals
            .iter()
            .filter(|(_, terminal)| terminal.state == AgentState::Working)
            .filter_map(|(terminal_id, terminal)| {
                let session = super::creation::terminal_agent_session_info(terminal)?;
                Some(Transcript {
                    terminal_id: terminal_id.clone(),
                    format: TranscriptFormat::for_agent(&session.agent)?,
                    path: session.transcript_path?,
                })
            })
            .collect()
    }

    /// Takes in what the reads found. A read of an agent that has stopped working since is
    /// dropped. Attention is worked out again when what an agent is running changed.
    pub(super) fn finish_running_command_checks(&mut self, reads: Vec<RunningCommandRead>) {
        self.running_command_check_in_flight = false;
        let mut changed = false;
        for read in reads {
            let working = self
                .state
                .terminals
                .get(&read.terminal_id)
                .is_some_and(|terminal| terminal.state == AgentState::Working);
            match read.command.filter(|_| working) {
                Some(command) => {
                    let previous = self
                        .running_commands
                        .insert(read.terminal_id, command.clone());
                    changed |= previous.as_ref() != Some(&command);
                }
                None => changed |= self.running_commands.remove(&read.terminal_id).is_some(),
            }
        }
        if changed {
            self.attention_schedule.mark_dirty();
        }
    }
}

/// The oldest command `transcript` shows its agent running whose start is known.
fn read_running_command(transcript: Transcript) -> RunningCommandRead {
    let command = match crate::transcript::running_shell_calls(
        transcript.format,
        Path::new(&transcript.path),
    ) {
        Ok(calls) => calls
            .into_iter()
            .filter_map(|call| {
                Some(RunningCommand {
                    started_at: call.started_at?,
                    call_id: call.call_id,
                    label: call.label,
                })
            })
            .min_by_key(|command| command.started_at),
        Err(err) => {
            tracing::debug!(
                path = %transcript.path,
                err = %err,
                "could not read an agent transcript for the commands it is running"
            );
            None
        }
    };
    RunningCommandRead {
        terminal_id: transcript.terminal_id,
        command,
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use std::time::SystemTime;

    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;

    /// Writes the transcript of a Claude Code turn, a prompt and then each of `calls` as a
    /// shell call that has no result: `(call id, command, when it was written)`. Returns where.
    pub(in crate::app) fn claude_transcript(
        name: &str,
        calls: &[(&str, &str, Option<SystemTime>)],
    ) -> std::path::PathBuf {
        let mut lines = vec![serde_json::json!({
            "type": "user",
            "uuid": "u1",
            "timestamp": "2025-10-09T08:00:00Z",
            "message": {"role": "user", "content": "go"},
        })
        .to_string()];
        for (n, (call_id, command, written_at)) in calls.iter().enumerate() {
            let mut line = serde_json::json!({
                "type": "assistant",
                "uuid": format!("a{n}"),
                "message": {
                    "id": format!("m{n}"),
                    "role": "assistant",
                    "stop_reason": "tool_use",
                    "content": [{
                        "type": "tool_use",
                        "id": call_id,
                        "name": "Bash",
                        "input": {"command": command},
                    }],
                },
            });
            if let Some(at) = written_at {
                let at = OffsetDateTime::from(*at).format(&Rfc3339).unwrap();
                line["timestamp"] = serde_json::json!(at);
            }
            lines.push(line.to_string());
        }
        let dir =
            std::env::temp_dir().join(format!("herdr-running-commands-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.jsonl"));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::agent_resume::{AgentSessionRef, PersistedAgentSession};
    use crate::app::AppPolicy;
    use crate::detect::Agent;
    use crate::work_items::test_support::FakeSource;
    use crate::work_items::WorkItems;
    use crate::workspace::Workspace;

    /// An app with `workspaces` workspaces of one terminal each, and work items enabled.
    fn app(workspaces: usize) -> App {
        let mut app = App::new(
            &crate::config::Config::default(),
            AppPolicy::TEST,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = (0..workspaces)
            .map(|n| Workspace::test_new(&format!("w{n}")))
            .collect();
        app.state.ensure_test_terminals();
        app.work_items = WorkItems::for_test(
            vec![FakeSource::with_items(Vec::new()) as Arc<_>],
            Instant::now(),
        );
        app
    }

    /// Makes the agent of workspace `ws_idx` an `agent` in `state`, whose transcript is at
    /// `transcript`.
    fn agent(
        app: &mut App,
        ws_idx: usize,
        agent: Agent,
        label: &str,
        state: AgentState,
        transcript: Option<&Path>,
    ) -> TerminalId {
        let pane_id = app.state.workspaces[ws_idx].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[ws_idx].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(Some(agent), state);
        if let Some(path) = transcript {
            terminal.set_persisted_agent_session(PersistedAgentSession {
                source: "test".into(),
                agent: label.into(),
                session_ref: AgentSessionRef::path(path.to_str().unwrap()).unwrap(),
            });
        }
        terminal_id
    }

    fn command(label: &str) -> RunningCommand {
        RunningCommand {
            call_id: "toolu_1".into(),
            started_at: SystemTime::now(),
            label: label.into(),
        }
    }

    fn found(app: &mut App, terminal_id: &TerminalId, command: Option<RunningCommand>) {
        app.finish_running_command_checks(vec![RunningCommandRead {
            terminal_id: terminal_id.clone(),
            command,
        }]);
    }

    /// Attention was just worked out.
    fn settle(app: &mut App) {
        let revision = app.work_items.revision();
        app.attention_schedule.done(revision, None);
    }

    fn attention_is_due(app: &App) -> bool {
        app.attention_schedule
            .is_due(Instant::now(), app.work_items.revision())
    }

    fn wait_for_read(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.running_command_check_in_flight {
            assert!(Instant::now() < deadline, "the read never finished");
            app.drain_all_internal_events();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn only_working_agents_with_a_transcript_in_a_format_herdr_reads_are_read() {
        let mut app = app(4);
        let path = Path::new("/tmp/session.jsonl");
        let working = agent(
            &mut app,
            0,
            Agent::Claude,
            "claude",
            AgentState::Working,
            Some(path),
        );
        agent(
            &mut app,
            1,
            Agent::Claude,
            "claude",
            AgentState::Idle,
            Some(path),
        );
        agent(
            &mut app,
            2,
            Agent::Codex,
            "codex",
            AgentState::Working,
            Some(path),
        );
        agent(
            &mut app,
            3,
            Agent::Claude,
            "claude",
            AgentState::Working,
            None,
        );

        let transcripts = app.working_agent_transcripts();
        assert_eq!(transcripts.len(), 1);
        assert_eq!(transcripts[0].terminal_id, working);
        assert_eq!(transcripts[0].format, TranscriptFormat::Claude);
        assert_eq!(transcripts[0].path, "/tmp/session.jsonl");
    }

    #[test]
    fn the_oldest_command_with_a_known_start_is_the_one_an_agent_runs() {
        let at =
            |minutes_ago: u64| UNIX_EPOCH + Duration::from_secs(1_760_000_000 - minutes_ago * 60);
        let path = test_support::claude_transcript(
            "oldest",
            &[
                ("toolu_unknown", "ls", None),
                ("toolu_old", "cargo test", Some(at(20))),
                ("toolu_new", "cargo build", Some(at(5))),
            ],
        );
        let read = read_running_command(Transcript {
            terminal_id: TerminalId::alloc(),
            format: TranscriptFormat::Claude,
            path: path.to_str().unwrap().into(),
        });
        assert_eq!(
            read.command,
            Some(RunningCommand {
                call_id: "toolu_old".into(),
                started_at: at(20),
                label: "cargo test".into(),
            })
        );
    }

    #[test]
    fn an_agent_whose_transcript_cannot_be_read_is_running_nothing() {
        let missing = std::env::temp_dir().join("herdr-running-commands-missing.jsonl");
        let read = read_running_command(Transcript {
            terminal_id: TerminalId::alloc(),
            format: TranscriptFormat::Claude,
            path: missing.to_str().unwrap().into(),
        });
        assert_eq!(read.command, None);
    }

    #[test]
    fn what_a_read_found_is_kept_for_a_working_agent_and_attention_is_due_when_it_changed() {
        let mut app = app(1);
        let terminal = agent(
            &mut app,
            0,
            Agent::Claude,
            "claude",
            AgentState::Working,
            None,
        );
        let ls = command("ls");

        settle(&mut app);
        found(&mut app, &terminal, Some(ls.clone()));
        assert_eq!(app.running_commands.get(&terminal), Some(&ls));
        assert!(attention_is_due(&app));

        // The same command again is nothing new.
        settle(&mut app);
        found(&mut app, &terminal, Some(ls));
        assert!(!attention_is_due(&app));

        let make = command("make");
        found(&mut app, &terminal, Some(make.clone()));
        assert_eq!(app.running_commands.get(&terminal), Some(&make));
        assert!(attention_is_due(&app));

        settle(&mut app);
        found(&mut app, &terminal, None);
        assert_eq!(app.running_commands.get(&terminal), None);
        assert!(attention_is_due(&app));

        settle(&mut app);
        found(&mut app, &terminal, None);
        assert!(!attention_is_due(&app));
    }

    #[test]
    fn a_read_that_comes_back_after_its_agent_stopped_working_is_dropped() {
        let mut app = app(1);
        let terminal = agent(&mut app, 0, Agent::Claude, "claude", AgentState::Idle, None);

        settle(&mut app);
        found(&mut app, &terminal, Some(command("ls")));
        assert_eq!(app.running_commands.get(&terminal), None);
        assert!(!attention_is_due(&app));
    }

    #[test]
    fn transcripts_are_read_once_a_minute_while_an_agent_works_and_forgotten_after() {
        let mut app = app(1);
        // Whole seconds, which the transcript writes back exactly.
        let started = UNIX_EPOCH
            + Duration::from_secs(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    - 1200,
            );
        let path = test_support::claude_transcript(
            "schedule",
            &[("toolu_1", "cargo test", Some(started))],
        );
        let terminal = agent(
            &mut app,
            0,
            Agent::Claude,
            "claude",
            AgentState::Working,
            Some(&path),
        );
        let now = Instant::now();

        // Armed, but nothing is read before the first minute is over.
        app.run_running_command_check(now);
        assert_eq!(app.running_command_check_at, Some(now + CHECK_INTERVAL));
        assert!(!app.running_command_check_in_flight);

        app.run_running_command_check(now + CHECK_INTERVAL);
        assert!(app.running_command_check_in_flight);
        assert_eq!(app.running_command_check_at, Some(now + 2 * CHECK_INTERVAL));
        wait_for_read(&mut app);
        assert_eq!(
            app.running_commands.get(&terminal),
            Some(&RunningCommand {
                call_id: "toolu_1".into(),
                started_at: started,
                label: "cargo test".into(),
            })
        );

        // Nothing works any more: nothing is scheduled, and what was found is forgotten.
        app.state
            .terminals
            .get_mut(&terminal)
            .unwrap()
            .set_detected_state(Some(Agent::Claude), AgentState::Idle);
        app.run_running_command_check(now + 2 * CHECK_INTERVAL);
        assert_eq!(app.running_command_check_at, None);
        assert!(app.running_commands.is_empty());
    }

    #[test]
    fn a_read_does_not_start_while_the_last_one_is_still_out() {
        let mut app = app(1);
        let path = test_support::claude_transcript("in-flight", &[("toolu_1", "ls", None)]);
        agent(
            &mut app,
            0,
            Agent::Claude,
            "claude",
            AgentState::Working,
            Some(&path),
        );
        let now = Instant::now();
        app.running_command_check_in_flight = true;
        app.running_command_check_at = Some(now);

        app.run_running_command_check(now);

        // Pushed out, not left due for the loop to spin on.
        assert_eq!(app.running_command_check_at, Some(now + CHECK_INTERVAL));
        std::thread::sleep(Duration::from_millis(100));
        assert!(app.event_rx.try_recv().is_err());
    }

    #[test]
    fn nothing_is_read_while_work_items_are_off() {
        let mut app = app(1);
        app.work_items = WorkItems::disabled();
        let path = test_support::claude_transcript("off", &[("toolu_1", "ls", None)]);
        agent(
            &mut app,
            0,
            Agent::Claude,
            "claude",
            AgentState::Working,
            Some(&path),
        );
        let now = Instant::now();

        app.run_running_command_check(now);
        app.run_running_command_check(now + CHECK_INTERVAL);

        assert_eq!(app.running_command_check_at, None);
        assert!(!app.running_command_check_in_flight);
    }
}
