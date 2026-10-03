use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::{AgentStatus, ReadFormat, ReadSource};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentReadParams {
    pub target: String,
    pub source: ReadSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(default)]
    pub format: ReadFormat,
    #[serde(default = "super::common::default_true")]
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentSendKeysParams {
    pub target: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentWaitParams {
    pub target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub until: Vec<AgentStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentPromptWaitOptions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub until: Vec<AgentStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) submission_deadline: Option<std::time::Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentRenameParams {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentViewSetParams {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<AgentViewFilter>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<AgentViewSort>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct AgentViewClearParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AgentViewFilter {
    All {
        filters: Vec<AgentViewFilter>,
    },
    Any {
        filters: Vec<AgentViewFilter>,
    },
    Not {
        filter: Box<AgentViewFilter>,
    },
    Eq {
        field: AgentViewField,
        value: AgentViewValue,
    },
    In {
        field: AgentViewField,
        values: Vec<AgentViewValue>,
    },
    Exists {
        field: AgentViewField,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AgentViewField {
    Builtin(AgentViewBuiltinField),
    Token { token: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentViewBuiltinField {
    Status,
    WorkspaceId,
    TabId,
    PaneId,
    Agent,
    Seen,
    StateChangeSeq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AgentViewValue {
    String(String),
    Bool(bool),
    Number(u64),
    Context { context: AgentViewContext },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentViewContext {
    CurrentWorkspaceId,
    CurrentTabId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentViewSort {
    pub field: AgentViewSortField,
    #[serde(default)]
    pub order: AgentViewSortOrder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AgentViewSortField {
    Builtin(AgentViewBuiltinSortField),
    Token { token: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentViewBuiltinSortField {
    WorkspaceOrder,
    TabOrder,
    PaneOrder,
    Attention,
    Status,
    Agent,
    Seen,
    StateChangeSeq,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentViewSortOrder {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentStartParams {
    pub name: String,
    pub kind: String,
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Startup timeout in milliseconds. Values must be greater than 3000 and at most 300000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentPromptParams {
    pub target: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<AgentPromptWaitOptions>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentInfo {
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title_stripped: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub screen_detection_skipped: bool,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub state_labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[schemars(schema_with = "super::common::metadata_token_values_schema")]
    pub tokens: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<AgentSessionInfo>,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub focused: bool,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub launch_pending: bool,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub interactive_ready: bool,
    #[serde(default)]
    pub state_change_seq: u64,
    /// The current idle transition completed work, independently of who has viewed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentSessionInfo {
    pub source: String,
    pub agent: String,
    pub kind: crate::agent_resume::AgentSessionRefKind,
    pub value: String,
    /// The session's transcript file, when the agent's integration reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

/// Whether an agent's last complete message could be read from its transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentLastMessageStatus {
    /// `text` holds the message.
    Available,
    /// The transcript has no finished assistant message yet.
    NoMessage,
    /// The agent's integration has not reported a transcript for its current session.
    NoTranscript,
    /// Herdr has no reader for this agent's transcript format.
    UnsupportedFormat,
    /// The transcript file could not be read.
    Unreadable,
    #[serde(other)]
    Unknown,
}

/// The last complete message of an agent, read from its transcript. Prompts that block the
/// agent are not in transcripts; read them from the screen with `agent.read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentLastMessageInfo {
    pub terminal_id: String,
    pub pane_id: String,
    /// The agent, e.g. "omp" or "claude".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub status: AgentLastMessageStatus,
    /// The final assistant message of the agent's last finished turn, as markdown, never
    /// truncated. Present when `status` is `available`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Why the model stopped, as the transcript records it, e.g. "stop", "end_turn", "aborted".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    /// When the message was written, as the transcript records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentActivityParams {
    pub target: String,
    /// A cursor from an earlier response. Only entries added or changed since then are
    /// returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// The most entries returned, from 1 to 500 (default 200). When the turn has more, the
    /// newest are kept and `truncated` is set. With `since`, when more entries changed than
    /// `limit`, the answer is a `reset` holding the newest `limit` entries of the whole turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Include the assistant's thinking as `thinking` entries (default false). Thinking the
    /// transcript redacted or encrypted is skipped. Keep it the same for as long as a cursor
    /// is used: a cursor does not bring back thinking written before it.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub include_thinking: bool,
}

/// Whether an agent's current turn could be read from its transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivityStatus {
    /// The transcript was read: for `agent.activity`, `turn`, `entries` and `cursor` describe
    /// the current turn; for `agent.history`, `turns` holds the page.
    Available,
    /// The transcript has no turn yet: no user prompt, and no message the session opened with.
    NoActivity,
    /// The agent's integration has not reported a transcript for its current session.
    NoTranscript,
    /// Herdr has no reader for this agent's transcript format.
    UnsupportedFormat,
    /// The transcript file could not be read.
    Unreadable,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentActivityTurn {
    /// The turn's stable id, the same string `agent.history` gives the same turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// When the turn's first entry, its prompt or its `context`, was written, as the transcript
    /// records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// The turn's final assistant message exists and no tool call is outstanding.
    pub finished: bool,
}

/// What an agent is doing in its current turn, read from its transcript: from the newest
/// user prompt onwards. Prompts that block the agent are not in transcripts; read them
/// from the screen with `agent.read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentActivityInfo {
    pub terminal_id: String,
    pub pane_id: String,
    /// The agent, e.g. "omp" or "claude".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub status: AgentActivityStatus,
    /// Present when `status` is `available`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<AgentActivityTurn>,
    /// The current turn's entries in order, or only those added or changed after `since`.
    /// An entry sent again replaces the earlier one with the same `id`.
    #[serde(default)]
    pub entries: Vec<AgentActivityEntry>,
    /// Pass back as `since` to receive only what changed. Present when `status` is
    /// `available`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// `since` no longer applies: a new turn started, the cursor is not one Herdr issued, or
    /// more entries changed than `limit` allows. `entries` then holds the current turn, its
    /// newest `limit` entries at most, and replaces what the client has.
    #[serde(default)]
    pub reset: bool,
    /// Older entries of the turn were left out because of `limit`.
    #[serde(default)]
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivityEntryKind {
    /// The user's prompt that started the turn.
    Prompt,
    /// Assistant text written before or between tool calls.
    Note,
    /// A tool call and, once it arrives, its result.
    Tool,
    /// The final assistant message of a finished turn.
    Message,
    /// The assistant's thinking, in the entries only when the request asked for it. Thinking
    /// the transcript redacted or encrypted is not in them.
    Thinking,
    /// Where the agent summarised the conversation so far. `text` holds the summary when the
    /// transcript has it.
    Compaction,
    /// The message a session opened with that is not the user's, such as the context a
    /// handoff gave the agent. It opens the session's first turn in place of a prompt; `text`
    /// holds it, never truncated.
    Context,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentActivityEntry {
    /// Stable across calls for the same entry.
    pub id: String,
    pub kind: AgentActivityEntryKind,
    /// As the transcript records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// The text of a `prompt`, `note`, `message`, `thinking`, `compaction` or `context`, as
    /// markdown and never truncated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Present when `kind` is `tool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<AgentActivityTool>,
    /// How many images the prompt or tool result carried. The images themselves are not sent.
    /// Absent when there were none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivityToolKind {
    Shell,
    Read,
    /// Edits, writes and creates files.
    Edit,
    Search,
    Web,
    /// Subagents and tasks.
    Agent,
    /// Asks the user; `question` holds the questions.
    Question,
    Todo,
    Other,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentActivityToolStatus {
    /// No result yet.
    Running,
    Succeeded,
    /// The result is marked as an error.
    Failed,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentActivityTool {
    /// As the transcript records it, e.g. "Bash" or "edit".
    pub name: String,
    pub kind: AgentActivityToolKind,
    /// One line of at most 120 characters, never empty.
    pub summary: String,
    /// The file path, command or URL the call acts on, at most 200 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub status: AgentActivityToolStatus,
    /// The end of the result: at most its last 5 lines and 600 characters, without
    /// terminal escape sequences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// Present when `kind` is `question`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<AgentActivityQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentActivityQuestion {
    pub questions: Vec<AgentActivityQuestionItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentActivityQuestionItem {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub options: Vec<AgentActivityQuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentActivityQuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentHistoryParams {
    pub target: String,
    /// The `before` of an earlier response: the turns just older than the turn it names are
    /// returned. Without it, the newest turns, the current one included.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// The most turns returned, from 1 to 20 (default 5). Herdr may return fewer to keep the
    /// response under about 512 KiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
    /// Include the assistant's thinking as `thinking` entries (default false). Thinking the
    /// transcript redacted or encrypted is skipped.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub include_thinking: bool,
}

/// One turn of an agent's transcript: a user prompt, or the `context` the session opened with,
/// and everything up to the next prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentHistoryTurn {
    /// Stable across calls, and the `turn.id` that `agent.activity` gives the same turn.
    pub id: String,
    /// When the turn's first entry, its prompt or its `context`, was written, as the transcript
    /// records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// The turn's final assistant message exists and no tool call is outstanding.
    pub finished: bool,
    /// In order: the prompt (or the `context`), then notes, thinking, tool calls, compactions
    /// and further prompts the user sent mid-turn, and the final `message` once the turn
    /// finished. Each entry has the `id` that `agent.activity` gives it. At most 200: when the
    /// turn has more, its prompt and its newest entries are kept.
    #[serde(default)]
    pub entries: Vec<AgentActivityEntry>,
    /// Entries between the prompt and the newest ones were left out because the turn has
    /// more than 200.
    #[serde(default)]
    pub truncated: bool,
}

/// A page of an agent's conversation, read from its transcript: whole turns, the current one
/// included, newest page first. Prompts that block the agent are not in transcripts; read
/// them from the screen with `agent.read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentHistoryInfo {
    pub terminal_id: String,
    pub pane_id: String,
    /// The agent, e.g. "omp" or "claude".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The same statuses as `agent.activity`. `no_activity` means the transcript has no turn
    /// yet.
    pub status: AgentActivityStatus,
    /// The page's turns, oldest first. At most the `turns` asked for, and fewer when that
    /// would make the response larger than about 512 KiB, but at least one when the
    /// transcript has any.
    #[serde(default)]
    pub turns: Vec<AgentHistoryTurn>,
    /// Pass back as `before` for the next older page. Absent when this page reaches the start
    /// of the transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    /// The `before` given does not apply: it names another transcript (after `/clear`, say),
    /// a turn that no longer exists, or is not a cursor Herdr issued. `turns` then holds the
    /// newest page and replaces what the client holds.
    #[serde(default)]
    pub reset: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
}

/// Something Herdr did outside the agent's session that the agent should know about, e.g.
/// marking its pull request ready for review from another client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentNoteInfo {
    /// One or two lines addressed to the agent.
    pub text: String,
    /// Unix time in seconds when Herdr queued it.
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentNotesAddParams {
    pub target: String,
    /// What the agent should know, e.g. "The user deployed staging from outside this
    /// session". At most 2000 characters.
    pub text: String,
}
